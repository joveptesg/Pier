use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use bollard::auth::DockerCredentials;
use tokio::process::Command;

use crate::config::PierConfig;
use crate::docker::auth::write_docker_config;

/// Base directory for compose stacks.
fn stacks_dir(config: &PierConfig) -> PathBuf {
    config.data_dir.join("stacks")
}

/// Auth map passed to compose CLI. `None` means "use Docker daemon defaults".
pub type ComposeAuth = Option<HashMap<String, DockerCredentials>>;

fn apply_auth_env(cmd: &mut Command, auth_dir: &Option<tempfile::TempDir>) {
    if let Some(dir) = auth_dir {
        cmd.env("DOCKER_CONFIG", dir.path());
    }
}

/// Docker network names a compose file declares as `external: true`.
///
/// Line-oriented on purpose — the rest of Pier manipulates compose YAML the
/// same way, and this only needs the top-level `networks:` block. Anything it
/// cannot read confidently is left out: a missed entry costs the operator the
/// old Compose error, while a false positive would block a deploy that works.
///
/// Understands the two spellings of a name override, since the docker network
/// is what must exist, not the key:
///
/// ```yaml
/// networks:
///   frontend:            # → "frontend"
///     external: true
///   backend:             # → "shared-net"
///     external: true
///     name: shared-net
///   legacy:              # → "old-net"  (Compose v2 form)
///     external:
///       name: old-net
/// ```
pub(crate) fn external_networks(yaml: &str) -> Vec<String> {
    let mut out: Vec<String> = parse_networks_block(yaml)
        .into_iter()
        .filter(|d| d.external)
        .map(|d| d.name.unwrap_or(d.key))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One entry of a compose top-level `networks:` block.
pub(crate) struct NetworkDecl {
    /// The YAML key — what `services.*.networks` entries refer to.
    pub key: String,
    /// Declared `external: true`, in either spelling.
    pub external: bool,
    /// Explicit `name:` override, when the file gives one.
    pub name: Option<String>,
}

/// Read the top-level `networks:` block.
///
/// The single parser behind both [`external_networks`] (which needs the docker
/// network names, to check they exist) and the deploy-time network injection
/// (which needs the *keys*, to rewrite what each service references).
pub(crate) fn parse_networks_block(yaml: &str) -> Vec<NetworkDecl> {
    let mut out = Vec::new();
    let mut in_networks = false;
    let mut current: Option<NetworkDecl> = None;

    fn flush(entry: Option<NetworkDecl>, out: &mut Vec<NetworkDecl>) {
        if let Some(decl) = entry {
            out.push(decl);
        }
    }

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();

        // A new top-level key ends the block we care about.
        if indent == 0 {
            flush(current.take(), &mut out);
            in_networks = trimmed == "networks:";
            continue;
        }
        if !in_networks {
            continue;
        }

        // One level in: a network name. Two or more: its properties.
        if indent <= 2 {
            flush(current.take(), &mut out);
            if let Some(key) = trimmed.strip_suffix(':') {
                current = Some(NetworkDecl {
                    key: key.trim().to_string(),
                    external: false,
                    name: None,
                });
            }
            continue;
        }

        let Some(entry) = current.as_mut() else {
            continue;
        };
        if trimmed == "external: true" {
            entry.external = true;
        } else if trimmed == "external:" {
            // Compose v2 `external: { name: … }` — the nested `name:` below is
            // picked up by the branch after this one.
            entry.external = true;
        } else if let Some(name) = trimmed.strip_prefix("name:") {
            let name = name.trim().trim_matches('"').trim_matches('\'');
            if !name.is_empty() {
                entry.name = Some(name.to_string());
            }
        }
    }
    flush(current.take(), &mut out);

    out
}

/// Refuse the deploy when a network the compose file calls `external` is not on
/// the host.
///
/// `docker compose up` catches this too — but only after pulling every image,
/// so a typo in a network name costs minutes and then reports a bare "declared
/// as external, but could not be found" with no hint of what does exist. This
/// runs first and names the alternatives.
///
/// Fails open: if `docker network ls` cannot be read, the deploy proceeds and
/// Compose stays the authority.
async fn ensure_external_networks(yaml: &str) -> Result<()> {
    let required = external_networks(yaml);
    if required.is_empty() {
        return Ok(());
    }

    let Ok(output) = Command::new("docker")
        .args(["network", "ls", "--format", "{{.Name}}"])
        .output()
        .await
    else {
        return Ok(());
    };
    if !output.status.success() {
        return Ok(());
    }

    let existing: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let missing: Vec<&String> = required
        .iter()
        .filter(|n| !existing.iter().any(|e| e == *n))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let missing_list = missing
        .iter()
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // Pier's own networks first — for a service in a project, the name it
    // actually wanted is almost always one of these.
    let mut candidates: Vec<&str> = existing
        .iter()
        .filter(|n| n.starts_with("pier-"))
        .map(|n| n.as_str())
        .collect();
    candidates.truncate(15);
    let hint = if candidates.is_empty() {
        String::new()
    } else {
        format!(" Existing Pier networks: {}.", candidates.join(", "))
    };
    anyhow::bail!(
        "compose declares network(s) as external that do not exist on this host: {missing_list}.{hint} \
         Create them with `docker network create <name>` or point the compose file at a network that exists."
    );
}

/// Write compose YAML to disk and run `docker compose up -d`.
pub async fn deploy_stack(
    name: &str,
    yaml_content: &str,
    config: &PierConfig,
    auth: ComposeAuth,
) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    tokio::fs::create_dir_all(&stack_dir).await?;

    let compose_file = stack_dir.join("docker-compose.yml");
    tokio::fs::write(&compose_file, yaml_content).await?;

    ensure_external_networks(yaml_content).await?;

    let auth_dir = auth
        .as_ref()
        .and_then(|a| write_docker_config(a).ok().flatten());

    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f"])
        .arg(&compose_file)
        .args(["up", "-d"])
        .current_dir(&stack_dir);
    apply_auth_env(&mut cmd, &auth_dir);

    let output = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !output.status.success() {
        anyhow::bail!("docker compose up failed: {combined}");
    }

    Ok(combined)
}

/// No output at all for this long means the pull is wedged, not merely slow.
///
/// `docker compose --progress plain` emits a progress line per layer several
/// times a second while bytes are moving, so a genuinely slow-but-alive pull
/// never goes quiet for minutes. Total silence means the connection died
/// without the process noticing — the case where Pier used to wait forever.
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Backstop for a process that keeps talking but never finishes.
const ABSOLUTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Same as [`deploy_stack`], but streams output to `progress` as it arrives and
/// refuses to hang.
///
/// [`deploy_stack`] collects stdout/stderr with `.output()`, so nothing is
/// observable until the process exits — a multi-gigabyte pull looks identical
/// to a hang, and a pull that genuinely wedges never ends at all. Here the
/// pipes are read line by line, and a gap longer than [`STALL_TIMEOUT`] kills
/// the child and fails with a reason instead of waiting.
///
/// `--progress plain` keeps the output line-oriented; the default renderer
/// emits terminal redraw sequences that are noise in a log pane.
pub async fn deploy_stack_with_progress(
    name: &str,
    yaml_content: &str,
    config: &PierConfig,
    auth: ComposeAuth,
    progress: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stack_dir = stacks_dir(config).join(name);
    tokio::fs::create_dir_all(&stack_dir).await?;

    let compose_file = stack_dir.join("docker-compose.yml");
    tokio::fs::write(&compose_file, yaml_content).await?;

    ensure_external_networks(yaml_content).await?;

    let auth_dir = auth
        .as_ref()
        .and_then(|a| write_docker_config(a).ok().flatten());

    // `--progress` is a global `docker compose` flag, not an `up` flag —
    // passing it after `up` fails with "unknown flag" on current Compose.
    let mut cmd = Command::new("docker");
    cmd.args(["compose", "--progress", "plain", "-f"])
        .arg(&compose_file)
        .args(["up", "-d"])
        .current_dir(&stack_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    apply_auth_env(&mut cmd, &auth_dir);

    let mut child = cmd.spawn()?;

    // Both pipe readers feed one channel so ordering stays roughly
    // chronological; the loop below ends when both readers have dropped it.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for pipe in [
        child.stdout.take().map(PipeKind::Out),
        child.stderr.take().map(PipeKind::Err),
    ]
    .into_iter()
    .flatten()
    {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            match pipe {
                PipeKind::Out(p) => {
                    let mut lines = BufReader::new(p).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
                PipeKind::Err(p) => {
                    let mut lines = BufReader::new(p).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(line_tx);

    let started = std::time::Instant::now();
    let mut combined = String::new();

    loop {
        let elapsed = started.elapsed();
        let Some(left) = ABSOLUTE_TIMEOUT.checked_sub(elapsed) else {
            let _ = child.kill().await;
            anyhow::bail!(
                "docker compose up exceeded the {}-minute ceiling and was stopped.\n{combined}",
                ABSOLUTE_TIMEOUT.as_secs() / 60
            );
        };

        match tokio::time::timeout(STALL_TIMEOUT.min(left), line_rx.recv()).await {
            Ok(Some(line)) => {
                combined.push_str(&line);
                combined.push('\n');
                let _ = progress.send(line);
            }
            // Both readers dropped the sender: the process closed its pipes.
            Ok(None) => break,
            Err(_) => {
                let _ = child.kill().await;
                anyhow::bail!(
                    "no output for {} minutes — the image pull looks stalled, so it was stopped. \
                     Check the registry is reachable from this server.\n{combined}",
                    STALL_TIMEOUT.as_secs() / 60
                );
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("docker compose up failed: {combined}");
    }

    Ok(combined)
}

/// Which pipe a reader task is draining. Only exists because `ChildStdout` and
/// `ChildStderr` are distinct types and the two readers are otherwise identical.
enum PipeKind {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// Write compose YAML and run `docker compose up -d --force-recreate --pull always` (no cache).
pub async fn deploy_stack_no_cache(
    name: &str,
    yaml_content: &str,
    config: &PierConfig,
    auth: ComposeAuth,
) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    tokio::fs::create_dir_all(&stack_dir).await?;

    let compose_file = stack_dir.join("docker-compose.yml");
    tokio::fs::write(&compose_file, yaml_content).await?;

    ensure_external_networks(yaml_content).await?;

    let auth_dir = auth
        .as_ref()
        .and_then(|a| write_docker_config(a).ok().flatten());

    // Build without cache if there's a build context
    let mut build_cmd = Command::new("docker");
    build_cmd
        .args(["compose", "-f"])
        .arg(&compose_file)
        .args(["build", "--no-cache"])
        .current_dir(&stack_dir);
    apply_auth_env(&mut build_cmd, &auth_dir);
    let _ = build_cmd.output().await;

    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f"])
        .arg(&compose_file)
        .args(["up", "-d", "--force-recreate", "--pull", "always"])
        .current_dir(&stack_dir);
    apply_auth_env(&mut cmd, &auth_dir);

    let output = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !output.status.success() {
        anyhow::bail!("docker compose up (no-cache) failed: {combined}");
    }

    Ok(combined)
}

/// Run `docker compose down` for a stack.
pub async fn down_stack(name: &str, config: &PierConfig) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    let compose_file = stack_dir.join("docker-compose.yml");

    if !compose_file.exists() {
        anyhow::bail!("Stack '{name}' not found");
    }

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_file)
        .arg("down")
        .current_dir(&stack_dir)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(format!("{stdout}{stderr}"))
}

/// Run `docker compose down -v` for a stack (removes named volumes too).
pub async fn down_stack_with_volumes(name: &str, config: &PierConfig) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    let compose_file = stack_dir.join("docker-compose.yml");

    if !compose_file.exists() {
        anyhow::bail!("Stack '{name}' not found");
    }

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_file)
        .args(["down", "-v"])
        .current_dir(&stack_dir)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(format!("{stdout}{stderr}"))
}

/// List all stacks by scanning the stacks directory.
#[allow(dead_code)]
pub async fn list_stacks_on_disk(config: &PierConfig) -> Result<Vec<String>> {
    let dir = stacks_dir(config);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut stacks = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let compose_file = entry.path().join("docker-compose.yml");
            if compose_file.exists() {
                if let Some(name) = entry.file_name().to_str() {
                    stacks.push(name.to_string());
                }
            }
        }
    }

    Ok(stacks)
}

/// Read compose YAML content for a stack.
#[allow(dead_code)]
pub async fn read_stack_yaml(name: &str, config: &PierConfig) -> Result<String> {
    let compose_file = stacks_dir(config).join(name).join("docker-compose.yml");
    Ok(tokio::fs::read_to_string(compose_file).await?)
}

/// Remove stack directory.
pub async fn remove_stack(name: &str, config: &PierConfig) -> Result<()> {
    let stack_dir = stacks_dir(config).join(name);
    if stack_dir.exists() {
        tokio::fs::remove_dir_all(stack_dir).await?;
    }
    Ok(())
}

/// Snapshot of recent stack logs. Wraps
/// `docker compose -f <compose> logs --tail <n> --no-color` so the
/// output looks like what an operator would see at the shell.
///
/// Returns the combined stdout+stderr verbatim (compose mixes per-
/// service prefixes into stdout already, so we don't need to merge by
/// hand). `tail` is capped at 5000 lines to prevent a malicious or
/// runaway request from streaming gigabytes back through axum.
pub async fn get_stack_logs(name: &str, config: &PierConfig, tail: u64) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    let compose_file = stack_dir.join("docker-compose.yml");
    if !compose_file.exists() {
        anyhow::bail!("Stack '{name}' not found");
    }
    let tail = tail.clamp(1, 5000);

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_file)
        .args(["logs", "--tail", &tail.to_string(), "--no-color"])
        .current_dir(&stack_dir)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!("{stdout}{stderr}"))
}

/// Stream `docker compose logs -f` into a WebSocket. The child process
/// is killed when the websocket closes or the caller aborts the future,
/// so a disconnected client never leaves a zombie `docker compose logs`
/// behind.
///
/// We intentionally don't reconnect on Docker stream end — unlike the
/// container-level streamer, compose's own `-f` already follows
/// restarts internally. If `docker compose` exits we surface that and
/// let the client decide to retry.
pub async fn stream_stack_logs_ws(
    name: &str,
    config: &PierConfig,
    mut socket: axum::extract::ws::WebSocket,
) {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stack_dir = stacks_dir(config).join(name);
    let compose_file = stack_dir.join("docker-compose.yml");
    if !compose_file.exists() {
        let _ = socket
            .send(axum::extract::ws::Message::Text(
                format!("error: stack '{name}' not found").into(),
            ))
            .await;
        return;
    }

    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-f"])
        .arg(&compose_file)
        .args(["logs", "-f", "--tail", "200", "--no-color"])
        .current_dir(&stack_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(axum::extract::ws::Message::Text(
                    format!("error: spawn docker compose logs: {e}").into(),
                ))
                .await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(128);

    // Pump stdout
    if let Some(out) = stdout {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
    }
    // Pump stderr (compose writes status messages here, e.g. "service X exited")
    if let Some(err) = stderr {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx.send(line).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(tx); // close channel when both pumps exit

    // Forward lines + watch the socket for disconnect. We don't need
    // to consume client→server messages, but we do need to react to
    // the half-close so the spawned child can be reaped.
    loop {
        tokio::select! {
            biased;
            line = rx.recv() => {
                match line {
                    Some(line) => {
                        if socket
                            .send(axum::extract::ws::Message::Text(line.into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break, // child exited
                }
            }
            msg = socket.recv() => {
                if msg.is_none()
                    || matches!(msg, Some(Err(_)) | Some(Ok(axum::extract::ws::Message::Close(_))))
                {
                    break;
                }
            }
        }
    }

    // kill_on_drop handles the cleanup, but call wait explicitly so we
    // don't leave defunct processes if drop happens during shutdown.
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::external_networks;

    /// Both spellings of an external network, plus the `name:` override that
    /// decides which docker network actually has to exist.
    #[test]
    fn external_networks_reads_every_declaration_form() {
        let yaml = "services:
  app:
    image: nginx
    networks:
      - frontend
networks:
  frontend:
    external: true
  backend:
    external: true
    name: shared-net
  legacy:
    external:
      name: old-net
";
        assert_eq!(
            external_networks(yaml),
            vec!["frontend", "old-net", "shared-net"]
        );
    }

    /// Nothing to check when the file declares no external network: a managed
    /// network, `external: false`, and a service-level `networks:` list must
    /// not be mistaken for one.
    #[test]
    fn external_networks_ignores_managed_and_service_level_networks() {
        let yaml = "services:
  app:
    image: nginx
    networks:
      - pier-net
networks:
  pier-net:
    driver: bridge
    name: custom
  other:
    external: false
";
        assert!(external_networks(yaml).is_empty());
    }

    /// The real-world miss this guards: the operator writes the project name
    /// where the network name goes.
    #[test]
    fn external_networks_returns_the_typo_verbatim() {
        let yaml = "services:
  languagetool:
    image: erikvl87/languagetool:latest
    networks:
      - pier-voxly
networks:
  pier-voxly:
    external: true
";
        assert_eq!(external_networks(yaml), vec!["pier-voxly"]);
    }
}
