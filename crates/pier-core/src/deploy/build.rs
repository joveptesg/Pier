use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use bollard::auth::DockerCredentials;
use bollard::query_parameters::BuildImageOptions;
use bollard::Docker;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::Full;

use crate::state::AppState;

/// Clone a git repo to a temporary directory.
///
/// `ssh_key_path` — if `Some`, the key file is used for SSH authentication
/// via `GIT_SSH_COMMAND`. Required for `git@host:owner/repo.git` clones
/// (Deploy Key flow). HTTPS clones ignore it.
pub async fn clone_repo(
    url: &str,
    branch: &str,
    dest: &Path,
    ssh_key_path: Option<&Path>,
) -> Result<String> {
    tokio::fs::create_dir_all(dest).await?;

    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["clone", "--depth", "1", "--branch", branch, url])
        .arg(dest.to_string_lossy().as_ref())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        );

    if let Some(key) = ssh_key_path {
        let key_str = key.to_string_lossy();
        cmd.env(
            "GIT_SSH_COMMAND",
            format!("ssh -i {key_str} -o StrictHostKeyChecking=no"),
        );
    }

    let output = cmd.output().await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !output.status.success() {
        anyhow::bail!("{combined}");
    }

    Ok(combined)
}

/// Build a Docker image from a Dockerfile in the given directory.
///
/// `context_dir` is the build context root (a per-service subdirectory for
/// monorepo services). `dockerfile_rel` is the Dockerfile path **relative to
/// that context** (usually `"Dockerfile"`).
///
/// `auth_map` provides registry credentials keyed by host — Bollard forwards
/// them as `X-Registry-Config` so that `FROM` pulls from private registries
/// succeed. Pass `None` to fall back to the Docker daemon's default auth.
pub async fn docker_build(
    docker: &Docker,
    context_dir: &Path,
    dockerfile_rel: &str,
    image_tag: &str,
    auth_map: Option<HashMap<String, DockerCredentials>>,
) -> Result<String> {
    // Create a tar archive of the build context
    let tar_bytes = create_tar_archive(context_dir).await?;

    let options = BuildImageOptions {
        t: Some(image_tag.to_string()),
        dockerfile: dockerfile_rel.to_string(),
        rm: true,
        forcerm: true,
        ..Default::default()
    };

    let body = http_body_util::Either::Left(Full::new(Bytes::from(tar_bytes)));
    let mut stream = docker.build_image(options, auth_map, Some(body));
    let mut log = String::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(s) = info.stream {
                    log.push_str(&s);
                }
                if let Some(err_detail) = &info.error_detail {
                    let msg = err_detail
                        .message
                        .as_deref()
                        .unwrap_or("Unknown build error");
                    log.push_str(&format!("ERROR: {msg}\n"));
                    anyhow::bail!("Build error: {msg}");
                }
            }
            Err(e) => {
                log.push_str(&format!("Stream error: {e}\n"));
                anyhow::bail!("Build stream error: {e}");
            }
        }
    }

    Ok(log)
}

/// Create a tar archive from a directory for Docker build context.
async fn create_tar_archive(dir: &Path) -> Result<Vec<u8>> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::fs;
        let mut ar = tar::Builder::new(Vec::new());

        fn add_dir_recursive(
            ar: &mut tar::Builder<Vec<u8>>,
            base: &Path,
            current: &Path,
        ) -> Result<()> {
            for entry in fs::read_dir(current)? {
                let entry = entry?;
                let path = entry.path();
                let name = path.strip_prefix(base)?;

                // Skip .git directory
                if name.starts_with(".git") {
                    continue;
                }

                if path.is_dir() {
                    add_dir_recursive(ar, base, &path)?;
                } else {
                    ar.append_path_with_name(&path, name)?;
                }
            }
            Ok(())
        }

        add_dir_recursive(&mut ar, &dir, &dir)?;
        Ok(ar.into_inner()?)
    })
    .await?
}

/// Generate a compose YAML for a built image.
pub fn generate_compose_for_image(
    _name: &str,
    stack_name: &str,
    image_tag: &str,
    state: &AppState,
    service_id: &str,
) -> String {
    // Read current port from DB
    let port: u16 = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT port FROM services WHERE id = ?1",
                [service_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
            .map(|p| p as u16)
        })
        .unwrap_or(3000);

    // Read container port + public exposure flag from port_allocations.
    // `public_port` (Some) → bind an extra `0.0.0.0:{public}:{container}` line
    // so the operator-toggled public port is reachable from outside the host
    // directly via Docker (no Traefik TCP routing).
    let (container_port, public_port): (u16, Option<u16>) = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT container_port, is_public, public_port \
                 FROM port_allocations WHERE service_id = ?1 LIMIT 1",
                [service_id],
                |row| {
                    let cp: i64 = row.get(0)?;
                    let is_pub: i64 = row.get(1)?;
                    let pp: Option<i64> = row.get(2)?;
                    Ok((
                        cp as u16,
                        if is_pub == 1 {
                            pp.map(|p| p as u16)
                        } else {
                            None
                        },
                    ))
                },
            )
            .ok()
        })
        .unwrap_or((3000, None));

    // Read network for this service
    let network_name: String = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT n.name FROM networks n JOIN services s ON s.network_id = n.id WHERE s.id = ?1",
                [service_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "pier-net".to_string());

    let public_line = match public_port {
        Some(p) if p != port => format!("\x20     - \"0.0.0.0:{p}:{container_port}\"\n"),
        _ => String::new(),
    };

    let mut yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   image: {image_tag}\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"127.0.0.1:{port}:{container_port}\"\n\
         {public_line}\
         \x20   env_file: .env\n\
         \x20   restart: unless-stopped\n\
         \x20   dns:\n\
         \x20     - 8.8.8.8\n\
         \x20     - 1.1.1.1\n\
         \x20   networks:\n\
         \x20     - {network_name}\n\
         \x20   labels:\n\
         \x20     pier.service.id: \"{service_id}\"\n"
    );
    yaml.push_str(&format!(
        "networks:\n\
         \x20 {network_name}:\n\
         \x20   external: true\n"
    ));
    if network_name != "pier-net" {
        yaml.push_str(
            "\x20 pier-net:\n\
             \x20   external: true\n",
        );
    }
    yaml
}

/// Parse a textarea-style KEY=VALUE block (one per line) into pairs.
///
/// Lines that are blank or have no `=` are skipped. Whitespace around the
/// key is trimmed; values are kept verbatim (including embedded `=` signs
/// and trailing whitespace, since some configs legitimately need spaces).
pub fn parse_kv_lines(blob: Option<&str>) -> Vec<(String, String)> {
    let Some(blob) = blob else {
        return Vec::new();
    };
    blob.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Build an OCI image from source using the `railpack` CLI.
///
/// Pier shells out to the `railpack` binary (zero-config builder by Railway,
/// successor to Nixpacks; talks to a moby/buildkit daemon over BUILDKIT_HOST).
/// Both prerequisites are provisioned by `install.sh` — see that script for
/// the BuildKit container setup and the PIER_BUILDKIT_MEMORY env var.
///
/// `repo_dir`     — path to a freshly cloned git working tree.
/// `image_tag`    — local tag the resulting image will be named with so the
///                  later `docker compose up` step can reference it.
/// `env_vars`     — build-time env passed as repeated `--env KEY=VALUE`.
/// `start_cmd`    — optional override for the container start command. When
///                  `None`, railpack auto-detects from the project (e.g.
///                  `npm start`, `python app.py`).
/// `log_sink`     — invoked once per stdout/stderr line; the caller is
///                  expected to batch writes into the `deployments.build_log`
///                  column (same flush-every-N-lines pattern as the
///                  docker-compose branch in `deploy::run_pipeline`).
pub async fn railpack_build(
    repo_dir: &Path,
    image_tag: &str,
    env_vars: &[(String, String)],
    start_cmd: Option<&str>,
    mut log_sink: impl FnMut(&str),
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    // Friendly upfront check — surface a clear message instead of the cryptic
    // "No such file or directory" from spawn() when the operator hasn't run
    // the updated install.sh yet.
    let which = tokio::process::Command::new("railpack")
        .arg("--version")
        .output()
        .await;
    match which {
        Ok(o) if o.status.success() => {}
        _ => anyhow::bail!(
            "railpack binary not found in PATH — run install.sh to provision it, \
             or install manually from https://github.com/railwayapp/railpack/releases"
        ),
    }

    let mut cmd = tokio::process::Command::new("railpack");
    cmd.arg("build").arg(repo_dir).args(["--name", image_tag]);

    for (k, v) in env_vars {
        cmd.args(["--env", &format!("{k}={v}")]);
    }
    if let Some(sc) = start_cmd {
        let sc = sc.trim();
        if !sc.is_empty() {
            cmd.args(["--start-cmd", sc]);
        }
    }

    // Merge stderr into stdout so progress + errors arrive in one ordered
    // stream — same trick as the docker-compose branch (`sh -c "... 2>&1"`).
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn railpack: {e}"))?;

    // Stream stdout
    if let Some(out) = child.stdout.take() {
        let mut reader = BufReader::new(out).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            log_sink(&line);
        }
    }
    // Drain stderr separately — railpack writes most progress to stdout but
    // BuildKit errors land here, so we surface them in the deployment log.
    if let Some(err) = child.stderr.take() {
        let mut reader = BufReader::new(err).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            log_sink(&line);
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow::anyhow!("wait railpack: {e}"))?;

    if !status.success() {
        anyhow::bail!(
            "railpack build failed (exit {}). Check BUILDKIT_HOST and ensure the buildkit container is running.",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_kv_lines;

    #[test]
    fn parse_kv_lines_basic() {
        let got = parse_kv_lines(Some("FOO=bar\nBAZ=qux"));
        assert_eq!(
            got,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn parse_kv_lines_skips_blank_and_comments() {
        let got = parse_kv_lines(Some(
            "\n# a comment\nFOO=bar\n   \n  # indented comment\nBAZ=qux\n",
        ));
        assert_eq!(
            got,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn parse_kv_lines_keeps_value_equals() {
        let got = parse_kv_lines(Some("URL=https://example.com/?a=1&b=2"));
        assert_eq!(
            got,
            vec![(
                "URL".to_string(),
                "https://example.com/?a=1&b=2".to_string()
            )]
        );
    }

    #[test]
    fn parse_kv_lines_skips_lines_without_equals() {
        let got = parse_kv_lines(Some("FOO=bar\nnotakv\nBAZ=qux"));
        assert_eq!(
            got,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn parse_kv_lines_none_returns_empty() {
        assert!(parse_kv_lines(None).is_empty());
    }
}
