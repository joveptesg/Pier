use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use sysinfo::System;

use crate::error::AppResult;
use crate::state::SharedState;

/// GET /api/v1/system/metrics
pub async fn metrics() -> AppResult<impl IntoResponse> {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let cpu_usage: f32 = sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>()
        / sys.cpus().len().max(1) as f32;

    let disks: Vec<serde_json::Value> = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name().to_str().unwrap_or(""),
                "mount": d.mount_point().to_str().unwrap_or(""),
                "total": d.total_space(),
                "available": d.available_space(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "cpu_usage": format!("{cpu_usage:.1}"),
        "cpu_count": sys.cpus().len(),
        "memory_total": total_memory,
        "memory_used": used_memory,
        "memory_percent": format!("{:.1}", (used_memory as f64 / total_memory.max(1) as f64) * 100.0),
        "uptime": System::uptime(),
        "hostname": System::host_name().unwrap_or_default(),
        "os": System::long_os_version().unwrap_or_default(),
        "disks": disks,
    })))
}

/// GET /api/v1/system/docker
pub async fn docker_info(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let version = state.docker.version().await?;
    let info = state.docker.info().await?;

    Ok(Json(serde_json::json!({
        "version": version.version,
        "api_version": version.api_version,
        "os": version.os,
        "arch": version.arch,
        "kernel_version": version.kernel_version,
        "containers": info.containers,
        "containers_running": info.containers_running,
        "containers_stopped": info.containers_stopped,
        "images": info.images,
        "storage_driver": info.driver,
    })))
}
