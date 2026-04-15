use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use sysinfo::System;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/system/metrics
pub async fn metrics() -> AppResult<impl IntoResponse> {
    let mut sys = System::new_all();
    sys.refresh_all();

    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let cpu_usage: f32 =
        sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / sys.cpus().len().max(1) as f32;

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

const GITHUB_RELEASE_URL: &str =
    "https://api.github.com/repos/joveptesg/Pier/releases/tags/latest";
const BINARY_ASSET_NAME: &str = "pier-linux-amd64";

/// GET /api/v1/system/update-check
pub async fn update_check() -> AppResult<impl IntoResponse> {
    let current_version = env!("CARGO_PKG_VERSION");

    // Get current binary modification time
    let bin_path = std::env::current_exe().unwrap_or_default();
    let bin_mtime = std::fs::metadata(&bin_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Fetch latest release from GitHub
    let client = reqwest::Client::builder()
        .user_agent("pier-updater")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client: {e}"))?;

    let resp = client
        .get(GITHUB_RELEASE_URL)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GitHub API: {e}"))?;

    if !resp.status().is_success() {
        return Ok(Json(serde_json::json!({
            "available": false,
            "current_version": current_version,
            "error": format!("GitHub API returned {}", resp.status()),
        })));
    }

    let release: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Parse release: {e}"))?;

    // Find the binary asset
    let assets = release["assets"].as_array();
    let asset = assets.and_then(|a| a.iter().find(|x| {
        x["name"].as_str().unwrap_or("") == BINARY_ASSET_NAME
    }));

    let (download_url, asset_updated, asset_size) = match asset {
        Some(a) => (
            a["browser_download_url"].as_str().unwrap_or("").to_string(),
            a["updated_at"].as_str().unwrap_or("").to_string(),
            a["size"].as_u64().unwrap_or(0),
        ),
        None => {
            return Ok(Json(serde_json::json!({
                "available": false,
                "current_version": current_version,
                "error": "Binary asset not found in release",
            })));
        }
    };

    // Compare: asset updated_at vs binary mtime
    let asset_ts = chrono::DateTime::parse_from_rfc3339(&asset_updated)
        .map(|dt| dt.timestamp() as u64)
        .unwrap_or(0);

    let available = asset_ts > bin_mtime;

    Ok(Json(serde_json::json!({
        "available": available,
        "current_version": current_version,
        "latest_build": asset_updated,
        "binary_date": bin_mtime,
        "download_url": download_url,
        "size": asset_size,
    })))
}

/// POST /api/v1/system/update
pub async fn update_now() -> AppResult<impl IntoResponse> {
    // Fetch release info first
    let client = reqwest::Client::builder()
        .user_agent("pier-updater")
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client: {e}"))?;

    let resp = client
        .get(GITHUB_RELEASE_URL)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GitHub API: {e}"))?;

    let release: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Parse release: {e}"))?;

    let download_url = release["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str().unwrap_or("") == BINARY_ASSET_NAME))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("Binary asset not found")))?
        .to_string();

    // Download binary
    tracing::info!("Downloading update from {download_url}");
    let bin_resp = client
        .get(&download_url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Download: {e}"))?;

    if !bin_resp.status().is_success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Download failed: {}",
            bin_resp.status()
        )));
    }

    let bytes = bin_resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("Read binary: {e}"))?;

    // Write to pier.new
    let bin_dir = std::path::PathBuf::from("/opt/pier/bin");
    let new_path = bin_dir.join("pier.new");
    let current_path = bin_dir.join("pier");
    let old_path = bin_dir.join("pier.old");

    tokio::fs::write(&new_path, &bytes)
        .await
        .map_err(|e| anyhow::anyhow!("Write pier.new: {e}"))?;

    // chmod +x
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| anyhow::anyhow!("chmod: {e}"))?;
    }

    // Atomic swap: pier → pier.old, pier.new → pier
    let _ = std::fs::remove_file(&old_path); // remove previous .old
    if current_path.exists() {
        std::fs::rename(&current_path, &old_path)
            .map_err(|e| anyhow::anyhow!("Backup current binary: {e}"))?;
    }
    std::fs::rename(&new_path, &current_path)
        .map_err(|e| anyhow::anyhow!("Replace binary: {e}"))?;

    tracing::info!("Update downloaded ({} bytes), restarting...", bytes.len());

    // Restart via systemctl (async, doesn't block response)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let _ = tokio::process::Command::new("systemctl")
            .args(["restart", "pier"])
            .output()
            .await;
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "Update installed, restarting...",
        "size": bytes.len(),
    })))
}
