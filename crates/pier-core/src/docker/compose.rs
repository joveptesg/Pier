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
