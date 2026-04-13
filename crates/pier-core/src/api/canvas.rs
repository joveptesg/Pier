use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::AppResult;
use crate::state::SharedState;

/// GET /api/v1/canvas — all data needed for canvas architect view.
pub async fn get_canvas(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Resources with network and server info
    let mut stmt = db.prepare(
        "SELECT s.id, s.name, s.status, s.catalog_id, s.category, s.port, s.image,
                s.network_id, n.name, s.server_id, s.project_id, s.env_json, p.name
         FROM services s
         LEFT JOIN networks n ON s.network_id = n.id
         LEFT JOIN projects p ON s.project_id = p.id
         ORDER BY s.name",
    )?;
    let resources: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "catalog_id": row.get::<_, Option<String>>(3)?,
                "category": row.get::<_, Option<String>>(4)?,
                "port": row.get::<_, Option<i64>>(5)?,
                "image": row.get::<_, Option<String>>(6)?,
                "network_id": row.get::<_, Option<String>>(7)?,
                "network_name": row.get::<_, Option<String>>(8)?,
                "server_id": row.get::<_, Option<String>>(9)?,
                "project_id": row.get::<_, Option<String>>(10)?,
                "env_json": row.get::<_, Option<String>>(11)?,
                "project_name": row.get::<_, Option<String>>(12)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Servers
    let mut stmt = db.prepare(
        "SELECT id, name, host, status, is_local, cpu_count, memory_total, docker_version, country, city, country_code
         FROM servers ORDER BY is_local DESC, name",
    )?;
    let servers: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "host": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "is_local": row.get::<_, i64>(4)? != 0,
                "cpu_count": row.get::<_, Option<i64>>(5)?,
                "memory_total": row.get::<_, Option<i64>>(6)?,
                "docker_version": row.get::<_, Option<String>>(7)?,
                "country": row.get::<_, Option<String>>(8)?,
                "city": row.get::<_, Option<String>>(9)?,
                "country_code": row.get::<_, Option<String>>(10)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Networks
    let mut stmt = db.prepare(
        "SELECT id, name, is_default FROM networks ORDER BY is_default DESC, name",
    )?;
    let networks: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "is_default": row.get::<_, i64>(2)? != 0,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Canvas positions
    let mut stmt = db.prepare("SELECT service_id, x, y FROM canvas_positions")?;
    let positions: std::collections::HashMap<String, serde_json::Value> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                serde_json::json!({
                    "x": row.get::<_, f64>(1)?,
                    "y": row.get::<_, f64>(2)?,
                }),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Enrich resources: fallback env_json from .env files on disk
    let data_dir = &state.config.data_dir;
    let resources: Vec<serde_json::Value> = resources
        .into_iter()
        .map(|mut r| {
            let env_json = r.get("env_json").and_then(|v| v.as_str()).unwrap_or("");
            if env_json.is_empty() || env_json == "null" || env_json == "{}" {
                // Try reading .env file from stack directory
                if let Some(name) = r.get("name").and_then(|v| v.as_str()) {
                    let env_path = data_dir.join("stacks").join(name).join(".env");
                    if let Ok(content) = std::fs::read_to_string(&env_path) {
                        let mut env_map = serde_json::Map::new();
                        for line in content.lines() {
                            let line = line.trim();
                            if line.is_empty() || line.starts_with('#') {
                                continue;
                            }
                            if let Some((key, val)) = line.split_once('=') {
                                let val = val.trim_matches('"').trim_matches('\'');
                                env_map.insert(
                                    key.trim().to_string(),
                                    serde_json::Value::String(val.to_string()),
                                );
                            }
                        }
                        if !env_map.is_empty() {
                            if let Ok(json_str) = serde_json::to_string(&env_map) {
                                r["env_json"] = serde_json::Value::String(json_str);
                            }
                        }
                    }
                }
            }
            r
        })
        .collect();

    // System metrics
    let sys = sysinfo::System::new_all();
    let cpu_percent = sys.global_cpu_usage();
    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let mem_percent = if mem_total > 0 {
        (mem_used as f64 / mem_total as f64 * 100.0) as f32
    } else {
        0.0
    };

    Ok(Json(serde_json::json!({
        "resources": resources,
        "servers": servers,
        "networks": networks,
        "positions": positions,
        "system": {
            "cpu_percent": cpu_percent,
            "memory_percent": mem_percent,
            "memory_used": mem_used,
            "memory_total": mem_total,
        }
    })))
}

/// PUT /api/v1/canvas/positions — save card positions after drag.
pub async fn save_positions(
    State(state): State<SharedState>,
    Json(body): Json<Vec<PositionUpdate>>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    for pos in &body {
        db.execute(
            "INSERT INTO canvas_positions (service_id, x, y, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(service_id) DO UPDATE SET x = ?2, y = ?3, updated_at = datetime('now')",
            rusqlite::params![pos.service_id, pos.x, pos.y],
        )?;
    }

    Ok(Json(serde_json::json!({"ok": true, "saved": body.len()})))
}

#[derive(Deserialize)]
pub struct PositionUpdate {
    pub service_id: String,
    pub x: f64,
    pub y: f64,
}
