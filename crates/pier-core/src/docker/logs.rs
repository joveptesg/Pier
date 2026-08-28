use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use bollard::query_parameters::{ListContainersOptions, LogsOptions};
use bollard::Docker;
use futures_util::StreamExt;

/// Resolve the container name/id the caller asked for into one Docker can act
/// on, tolerating the docker-compose naming drift.
///
/// The Logs route receives a name the UI guessed. For raw docker-compose
/// stacks that guess is often the compose *project* name (`pier-{slug}`), but
/// docker-compose actually creates `pier-{slug}-{service}-1`. When a direct
/// inspect fails we fall back to a `list_containers` scan and match by exact
/// name, compose-child prefix (`pier-{slug}-…`), or `com.docker.compose.project`
/// label. Prefer returning the container's *name* (stable across restarts) so
/// the follow-stream keeps working after a container is recreated.
///
/// Returns `None` only when no container matches.
async fn resolve_container(docker: &Docker, id: &str) -> Option<String> {
    // Fast path: the caller's name/id works as-is.
    if docker.inspect_container(id, None).await.is_ok() {
        return Some(id.to_string());
    }

    let list = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
        .ok()?;

    let prefix = format!("{id}-");
    let found = list.into_iter().find(|c| {
        let name_match = c.names.as_ref().is_some_and(|names| {
            names.iter().any(|n| {
                let n = n.trim_start_matches('/');
                n == id || n.starts_with(&prefix)
            })
        });
        let project_match = c
            .labels
            .as_ref()
            .and_then(|l| l.get("com.docker.compose.project"))
            .is_some_and(|p| p == id);
        name_match || project_match
    })?;

    found
        .names
        .as_ref()
        .and_then(|names| names.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .or(found.id)
}
/// Whether a container matching this name/id exists right now.
///
/// Lets a handler tell "not created yet" apart from a real Docker failure. A
/// service whose image is still downloading has no container, and reporting
/// that as an internal error tells the operator nothing about what to do.
pub async fn container_exists(docker: &Docker, id: &str) -> bool {
    resolve_container(docker, id).await.is_some()
}

/// Stream container logs to a WebSocket connection.
/// Resilient: retries on Docker stream errors (e.g., container restart).
pub async fn stream_logs_ws(docker: &Docker, container_id: &str, mut socket: WebSocket) {
    // Resolve the guessed name to a real container (docker-compose drift); if
    // nothing matches there's nothing to stream.
    let container_id = match resolve_container(docker, container_id).await {
        Some(target) => target,
        None => return,
    };
    let mut retry_count = 0u32;

    loop {
        let options = LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            tail: "0".to_string(), // only new lines (old lines loaded via HTTP)
            timestamps: true,
            ..Default::default()
        };

        let mut stream = docker.logs(&container_id, Some(options));

        while let Some(result) = stream.next().await {
            match result {
                Ok(output) => {
                    retry_count = 0;
                    let text = output.to_string().trim_end().to_string();
                    if !text.is_empty() && socket.send(Message::Text(text.into())).await.is_err() {
                        return; // client disconnected
                    }
                }
                Err(_) => break, // stream ended, will retry
            }
        }

        // Docker stream ended (container restart, etc.) — retry
        retry_count += 1;
        if retry_count > 120 {
            return; // give up after 120 retries (~4 min)
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Check if WS client still connected
        if socket.send(Message::Ping(vec![].into())).await.is_err() {
            return;
        }
    }
}

/// Get last N lines of container logs.
pub async fn get_logs(
    docker: &Docker,
    container_id: &str,
    tail: u64,
    timestamps: bool,
) -> Result<Vec<String>> {
    // Resolve the (possibly guessed) name to a real container, tolerating
    // docker-compose naming drift instead of 500-ing.
    let target = resolve_container(docker, container_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("Container '{container_id}' not found"))?;

    let options = LogsOptions {
        follow: false,
        stdout: true,
        stderr: true,
        tail: tail.to_string(),
        timestamps,
        ..Default::default()
    };

    let mut stream = docker.logs(&target, Some(options));
    let mut lines = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(output) => {
                let line = output.to_string().trim_end().to_string();
                if !line.trim().is_empty() {
                    lines.push(line);
                }
            }
            Err(e) => {
                tracing::warn!("Log stream error for {container_id}: {e}");
                break;
            }
        }
    }

    Ok(lines)
}
