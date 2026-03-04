use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use bollard::query_parameters::LogsOptions;
use bollard::Docker;
use futures_util::StreamExt;

/// Stream container logs to a WebSocket connection.
pub async fn stream_logs_ws(docker: &Docker, container_id: &str, mut socket: WebSocket) {
    let options = LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        tail: "100".to_string(),
        timestamps: true,
        ..Default::default()
    };

    let mut stream = docker.logs(container_id, Some(options));

    while let Some(result) = stream.next().await {
        match result {
            Ok(output) => {
                let text = output.to_string();
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                let _ = socket
                    .send(Message::Text(format!("Error: {e}").into()))
                    .await;
                break;
            }
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
            Ok(output) => lines.push(output.to_string()),
            Err(_) => break,
        }
    }

    Ok(lines)
}
