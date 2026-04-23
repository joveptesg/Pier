use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use bollard::query_parameters::LogsOptions;
use bollard::Docker;
use futures_util::StreamExt;

/// Stream container logs to a WebSocket connection.
/// Resilient: retries on Docker stream errors (e.g., container restart).
pub async fn stream_logs_ws(docker: &Docker, container_id: &str, mut socket: WebSocket) {
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

        let mut stream = docker.logs(container_id, Some(options));

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
    // First check if container exists
    if let Err(e) = docker.inspect_container(container_id, None).await {
        anyhow::bail!("Container '{container_id}' not found: {e}");
    }

    let options = LogsOptions {
        follow: false,
        stdout: true,
        stderr: true,
        tail: tail.to_string(),
        timestamps,
        ..Default::default()
    };

    let mut stream = docker.logs(container_id, Some(options));
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
