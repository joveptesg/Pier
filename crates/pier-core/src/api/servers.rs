use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: i64,
    pub ssh_user: Option<String>,
    pub ssh_port: Option<i64>,
}

fn default_port() -> i64 {
    3001
}

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    pub agent_token: String,
    pub os_info: Option<String>,
    pub cpu_count: Option<i64>,
    pub memory_total: Option<i64>,
    pub docker_version: Option<String>,
}

/// GET /api/v1/servers
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, host, port, status, last_heartbeat, os_info, cpu_count, memory_total, docker_version, is_local, created_at
         FROM servers ORDER BY is_local DESC, created_at ASC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "host": row.get::<_, String>(2)?,
                "port": row.get::<_, i64>(3)?,
                "status": row.get::<_, String>(4)?,
                "last_heartbeat": row.get::<_, Option<String>>(5)?,
                "os_info": row.get::<_, Option<String>>(6)?,
                "cpu_count": row.get::<_, Option<i64>>(7)?,
                "memory_total": row.get::<_, Option<i64>>(8)?,
                "docker_version": row.get::<_, Option<String>>(9)?,
                "is_local": row.get::<_, bool>(10)?,
                "created_at": row.get::<_, String>(11)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/servers
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateServerRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.host.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Name and host are required".into(),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let agent_token = catalog::generate_password(32);

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO servers (id, name, host, port, agent_token, ssh_user, ssh_port)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.host.trim(),
            body.port,
            agent_token,
            body.ssh_user,
            body.ssh_port.unwrap_or(22)
        ],
    )?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "agent_token": agent_token
    })))
}

/// DELETE /api/v1/servers/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM servers WHERE id = ?1 AND is_local = 0", [&id])?;
    if rows == 0 {
        return Err(AppError::BadRequest(
            "Server not found or is local (cannot delete)".into(),
        ));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/servers/{id}/test
pub async fn test_connection(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host, port, agent_token FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Server {id} not found")))?
    };

    let url = format!("http://{}:{}/health", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(format!(
                "Cannot connect to agent at {url}: {e}"
            ))
        })?;

    if resp.status().is_success() {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
            [&id],
        )?;
        Ok(Json(
            serde_json::json!({"ok": true, "message": "Agent is online"}),
        ))
    } else {
        Err(AppError::BadRequest(format!(
            "Agent responded with status: {}",
            resp.status()
        )))
    }
}

/// GET /api/v1/servers/{id}/metrics
pub async fn metrics(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host, port, agent_token FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Server {id} not found")))?
    };

    let url = format!("http://{}:{}/metrics", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Agent unreachable: {e}")))?;

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON parse: {e}")))?;

    Ok(Json(data))
}

/// POST /api/v1/servers/heartbeat (public — uses token auth)
pub async fn heartbeat(
    State(state): State<SharedState>,
    Json(body): Json<HeartbeatRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'),
         os_info = COALESCE(?2, os_info), cpu_count = COALESCE(?3, cpu_count),
         memory_total = COALESCE(?4, memory_total), docker_version = COALESCE(?5, docker_version),
         updated_at = datetime('now')
         WHERE agent_token = ?1",
        rusqlite::params![
            body.agent_token,
            body.os_info,
            body.cpu_count,
            body.memory_total,
            body.docker_version
        ],
    )?;

    if rows == 0 {
        return Err(AppError::Unauthorized);
    }

    Ok(Json(serde_json::json!({"ok": true})))
}
