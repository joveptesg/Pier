use std::path::Path;

use anyhow::Result;
use bollard::query_parameters::BuildImageOptions;
use bollard::Docker;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::Full;

use crate::state::AppState;

/// Clone a git repo to a temporary directory.
pub async fn clone_repo(url: &str, branch: &str, dest: &Path) -> Result<String> {
    tokio::fs::create_dir_all(dest).await?;

    let output = tokio::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch, url])
        .arg(dest.to_string_lossy().as_ref())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !output.status.success() {
        anyhow::bail!("{combined}");
    }

    Ok(combined)
}

/// Build a Docker image from a Dockerfile in the given directory.
pub async fn docker_build(docker: &Docker, context_dir: &Path, image_tag: &str) -> Result<String> {
    // Create a tar archive of the build context
    let tar_bytes = create_tar_archive(context_dir).await?;

    let options = BuildImageOptions {
        t: Some(image_tag.to_string()),
        rm: true,
        forcerm: true,
        ..Default::default()
    };

    let body = http_body_util::Either::Left(Full::new(Bytes::from(tar_bytes)));
    let mut stream = docker.build_image(options, None, Some(body));
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

    // Read container port from port_allocations
    let container_port: u16 = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT container_port FROM port_allocations WHERE service_id = ?1 LIMIT 1",
                [service_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|p| p as u16)
        })
        .unwrap_or(3000);

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

    let mut yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   image: {image_tag}\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"127.0.0.1:{port}:{container_port}\"\n\
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
        yaml.push_str(&format!(
            "\x20 pier-net:\n\
             \x20   external: true\n"
        ));
    }
    yaml
}
