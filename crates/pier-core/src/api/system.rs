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

/// GET /api/v1/system/disk-usage — Docker disk usage breakdown via CLI
pub async fn disk_usage(State(_state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let output = tokio::process::Command::new("docker")
        .args(["system", "df", "-v", "--format", "{{json .}}"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("docker system df: {e}"))?;

    let raw = String::from_utf8_lossy(&output.stdout);
    // Output is one big JSON object with Images[], Containers[], Volumes[], BuildCache[]
    let df: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("Parse docker df: {e}"))?;

    let mut total: u64 = 0;

    let images: Vec<serde_json::Value> = df["Images"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|img| {
            let size = parse_docker_size(img["Size"].as_str().unwrap_or("0"));
            total += size;
            let repo = img["Repository"].as_str().unwrap_or("<none>");
            let tag = img["Tag"].as_str().unwrap_or("latest");
            serde_json::json!({ "name": format!("{repo}:{tag}"), "size": size })
        })
        .collect();

    let containers: Vec<serde_json::Value> = df["Containers"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|c| {
            let size = parse_docker_size(c["Size"].as_str().unwrap_or("0"));
            total += size;
            let name = c["Names"].as_str().unwrap_or("?");
            serde_json::json!({ "name": name, "size": size })
        })
        .collect();

    let volumes: Vec<serde_json::Value> = df["Volumes"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|v| {
            let size = parse_docker_size(v["Size"].as_str().unwrap_or("0"));
            total += size;
            let name = v["Name"].as_str().unwrap_or("?");
            let short = if name.len() > 40 { format!("{}...", &name[..37]) } else { name.to_string() };
            serde_json::json!({ "name": short, "size": size })
        })
        .collect();

    let build_cache_size: u64 = df["BuildCache"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .map(|b| parse_docker_size(b["Size"].as_str().unwrap_or("0")))
        .sum();
    total += build_cache_size;

    // Sort by size descending
    let mut images = images;
    let mut containers = containers;
    let mut volumes = volumes;
    images.sort_by(|a, b| b["size"].as_u64().cmp(&a["size"].as_u64()));
    containers.sort_by(|a, b| b["size"].as_u64().cmp(&a["size"].as_u64()));
    volumes.sort_by(|a, b| b["size"].as_u64().cmp(&a["size"].as_u64()));

    Ok(Json(serde_json::json!({
        "images": images,
        "containers": containers,
        "volumes": volumes,
        "build_cache_size": build_cache_size,
        "total": total,
    })))
}

/// GET /api/v1/system/cleanup-info — sizes of cleanable Docker data
/// GET /api/v1/system/info — version, build date, uptime
pub async fn info(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let version = env!("CARGO_PKG_VERSION");

    // Build date from binary mtime
    let build_date = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()
        })
        .unwrap_or_else(|| "Unknown".to_string());

    // Uptime from started_at in AppState
    let uptime_seconds = state.started_at.elapsed().as_secs();

    Ok(Json(serde_json::json!({
        "version": version,
        "build_date": build_date,
        "uptime_seconds": uptime_seconds,
    })))
}

pub async fn cleanup_info() -> AppResult<impl IntoResponse> {
    // docker system df (summary, not verbose)
    let output = tokio::process::Command::new("docker")
        .args(["system", "df", "--format", "{{json .}}"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("docker system df: {e}"))?;

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut build_cache: u64 = 0;
    let mut images_reclaimable: u64 = 0;
    let mut containers_size: u64 = 0;

    // Each line is a JSON object for a section (Images, Containers, Local Volumes, Build Cache)
    for line in raw.lines() {
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
            let typ = obj["Type"].as_str().unwrap_or("");
            let reclaimable = obj["Reclaimable"].as_str().unwrap_or("0B");
            // Parse reclaimable: "1.2GB (50%)" → extract size before "("
            let size_part = reclaimable.split('(').next().unwrap_or("0B").trim();
            match typ {
                "Images" => images_reclaimable = parse_docker_size(size_part),
                "Containers" => containers_size = parse_docker_size(obj["Size"].as_str().unwrap_or("0B")),
                "Build Cache" => build_cache = parse_docker_size(size_part),
                _ => {}
            }
        }
    }

    Ok(Json(serde_json::json!({
        "build_cache": build_cache,
        "images_reclaimable": images_reclaimable,
        "containers_size": containers_size,
    })))
}

/// POST /api/v1/system/cleanup — run cleanup for specified targets
pub async fn cleanup(Json(body): Json<serde_json::Value>) -> AppResult<impl IntoResponse> {
    let targets = body["targets"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    let mut results = serde_json::Map::new();

    for target in &targets {
        let (cmd, args): (&str, &[&str]) = match *target {
            "build_cache" => ("docker", &["builder", "prune", "-f"]),
            "images" => ("docker", &["image", "prune", "-f"]),
            "containers" => ("docker", &["container", "prune", "-f"]),
            _ => continue,
        };

        match tokio::process::Command::new(cmd).args(args).output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                tracing::info!("Cleanup {target}: {}", stdout.trim());
                results.insert(
                    target.to_string(),
                    serde_json::json!({ "ok": true, "output": stdout.trim() }),
                );
            }
            Err(e) => {
                tracing::warn!("Cleanup {target} failed: {e}");
                results.insert(
                    target.to_string(),
                    serde_json::json!({ "ok": false, "error": e.to_string() }),
                );
            }
        }
    }

    Ok(Json(serde_json::json!({ "results": results })))
}

/// PUT /api/v1/system/cleanup-settings — save cleanup preferences
pub async fn cleanup_settings_update(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let keys = [
        "cleanup.enabled",
        "cleanup.interval_hours",
        "cleanup.prune_build_cache",
        "cleanup.prune_images",
        "cleanup.prune_containers",
    ];

    for key in &keys {
        let short = key.strip_prefix("cleanup.").unwrap_or(key);
        if let Some(val) = body.get(short).and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| Some(v.to_string()))
        }) {
            db.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, val],
            )?;
        }
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// GET /api/v1/system/cleanup-settings — read cleanup preferences
pub async fn cleanup_settings_get(
    State(state): State<SharedState>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let get = |key: &str| -> String {
        db.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap_or_default()
    };

    Ok(Json(serde_json::json!({
        "enabled": get("cleanup.enabled") == "true",
        "interval_hours": get("cleanup.interval_hours").parse::<u32>().unwrap_or(24),
        "prune_build_cache": get("cleanup.prune_build_cache") != "false",
        "prune_images": get("cleanup.prune_images") != "false",
        "prune_containers": get("cleanup.prune_containers") == "true",
    })))
}

fn parse_docker_size(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() || s == "0B" || s == "0" { return 0; }
    let (num_str, unit) = if let Some(rest) = s.strip_suffix("GB") {
        (rest, 1_073_741_824u64)
    } else if let Some(rest) = s.strip_suffix("MB") {
        (rest, 1_048_576u64)
    } else if let Some(rest) = s.strip_suffix("kB").or_else(|| s.strip_suffix("KB")) {
        (rest, 1024u64)
    } else if let Some(rest) = s.strip_suffix('B') {
        (rest, 1u64)
    } else {
        (s, 1u64)
    };
    num_str.trim().parse::<f64>().unwrap_or(0.0) as u64 * unit
}

const GITHUB_RELEASE_URL: &str =
    "https://api.github.com/repos/joveptesg/Pier/releases/tags/latest";
const BINARY_ASSET_NAME: &str = "pier-linux-amd64";

/// GET /api/v1/system/update-settings
pub async fn update_settings(
    State(state): State<SharedState>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mode = db
        .query_row("SELECT value FROM settings WHERE key = 'update.mode'", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|_| "notify".to_string());
    let auto_check = db
        .query_row("SELECT value FROM settings WHERE key = 'update.auto_check'", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|_| "true".to_string()) == "true";

    Ok(Json(serde_json::json!({
        "mode": mode,
        "auto_check": auto_check,
    })))
}

/// PUT /api/v1/system/update-settings
pub async fn save_update_settings(
    State(state): State<SharedState>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    if let Some(mode) = body.get("mode").and_then(|v| v.as_str()) {
        if matches!(mode, "auto" | "notify" | "manual") {
            db.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('update.mode', ?1)",
                [mode],
            )?;
        }
    }
    if let Some(auto_check) = body.get("auto_check").and_then(|v| v.as_bool()) {
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('update.auto_check', ?1)",
            [if auto_check { "true" } else { "false" }],
        )?;
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

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
