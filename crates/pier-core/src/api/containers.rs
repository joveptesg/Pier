use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::docker;
use crate::error::AppResult;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub all: bool,
}

#[derive(Deserialize)]
pub struct LogParams {
    #[serde(default = "default_tail")]
    pub tail: u64,
    #[serde(default)]
    pub timestamps: bool,
}

fn default_tail() -> u64 {
    100
}

/// GET /api/v1/containers
pub async fn list(
    State(state): State<SharedState>,
    Query(params): Query<ListParams>,
) -> AppResult<impl IntoResponse> {
    let containers = docker::containers::list_containers(&state.docker, params.all).await?;
    Ok(Json(containers))
}

/// GET /api/v1/containers/:id
pub async fn inspect(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let info = docker::containers::inspect_container(&state.docker, &id).await?;
    Ok(Json(info))
}

/// POST /api/v1/containers/:id/start
pub async fn start(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    docker::containers::start_container(&state.docker, &id).await?;
    Ok(Json(serde_json::json!({"ok": true, "action": "started"})))
}

/// POST /api/v1/containers/:id/stop
pub async fn stop(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    docker::containers::stop_container(&state.docker, &id).await?;
    Ok(Json(serde_json::json!({"ok": true, "action": "stopped"})))
}

/// POST /api/v1/containers/:id/restart
pub async fn restart(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    docker::containers::restart_container(&state.docker, &id).await?;
    Ok(Json(serde_json::json!({"ok": true, "action": "restarted"})))
}

/// DELETE /api/v1/containers/:id
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    docker::containers::remove_container(&state.docker, &id, true).await?;
    Ok(Json(serde_json::json!({"ok": true, "action": "removed"})))
}

/// GET /api/v1/containers/:id/logs
pub async fn logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(params): Query<LogParams>,
) -> AppResult<impl IntoResponse> {
    let lines = docker::logs::get_logs(&state.docker, &id, params.tail, params.timestamps).await?;
    Ok(Json(lines))
}

/// GET /api/v1/containers/:id/logs/ws — WebSocket log streaming
pub async fn logs_ws(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let docker = state.docker.clone();
    ws.on_upgrade(move |socket| async move {
        docker::logs::stream_logs_ws(&docker, &id, socket).await;
    })
}

/// GET /api/v1/containers/:id/stats
pub async fn stats(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let stats = docker::containers::container_stats(&state.docker, &id).await?;
    Ok(Json(stats))
}

/// GET /api/v1/containers/all-stats — memory/cpu stats for all running containers.
pub async fn all_stats(
    State(state): State<SharedState>,
) -> AppResult<impl IntoResponse> {
    let containers = docker::containers::list_containers(&state.docker, false).await?;
    let mut results = Vec::new();

    for c in &containers {
        let name = &c.name;
        if let Ok(stats) = docker::containers::container_stats(&state.docker, name).await {
            results.push(serde_json::json!({
                "name": name,
                "image": c.image,
                "status": c.status,
                "cpu_percent": stats["cpu_percent"],
                "memory_usage": stats["memory_usage"],
                "memory_limit": stats["memory_limit"],
                "memory_percent": stats["memory_percent"],
            }));
        }
    }

    // Sort by memory usage descending
    results.sort_by(|a, b| {
        let ma = a["memory_usage"].as_u64().unwrap_or(0);
        let mb = b["memory_usage"].as_u64().unwrap_or(0);
        mb.cmp(&ma)
    });

    Ok(Json(results))
}
