use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateStackRequest {
    pub name: String,
    pub yaml: String,
}

#[derive(Deserialize)]
pub struct UpdateStackRequest {
    pub yaml: String,
}

/// GET /api/v1/stacks
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, name, compose_content, status, created_at FROM services WHERE service_type = 'compose'"
    )?;

    let stacks: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "has_yaml": row.get::<_, Option<String>>(2)?.is_some(),
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(stacks))
}

/// POST /api/v1/stacks
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateStackRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.yaml.trim().is_empty() {
        return Err(AppError::BadRequest("Name and YAML are required".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO services (id, name, service_type, compose_content, status)
         VALUES (?1, ?2, 'compose', ?3, 'created')",
        rusqlite::params![id, body.name.trim(), body.yaml],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// GET /api/v1/stacks/:id
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let result = db.query_row(
        "SELECT id, name, compose_content, status FROM services WHERE id = ?1 AND service_type = 'compose'",
        [&id],
        |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "yaml": row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "status": row.get::<_, String>(3)?,
            }))
        },
    ).map_err(|_| AppError::NotFound(format!("Stack {id} not found")))?;

    Ok(Json(result))
}

/// PUT /api/v1/stacks/:id
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateStackRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "UPDATE services SET compose_content = ?1, updated_at = datetime('now')
         WHERE id = ?2 AND service_type = 'compose'",
        rusqlite::params![body.yaml, id],
    )?;

    if rows == 0 {
        return Err(AppError::NotFound(format!("Stack {id} not found")));
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/stacks/:id/deploy
pub async fn deploy(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, compose_content FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Stack {id} not found")))?
    };

    let yaml = yaml.ok_or_else(|| AppError::BadRequest("Stack has no YAML content".into()))?;

    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    let output = docker::deploy_service_stack(&state, &id, &name, &yaml, auth).await?;

    // Update status
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'running', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    Ok(Json(serde_json::json!({"ok": true, "output": output})))
}

/// POST /api/v1/stacks/:id/down
pub async fn down(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| AppError::NotFound(format!("Stack {id} not found")))?
    };

    let output = docker::compose::down_stack(&name, &state.config).await?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'stopped', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    Ok(Json(serde_json::json!({"ok": true, "output": output})))
}

/// DELETE /api/v1/stacks/:id
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| AppError::NotFound(format!("Stack {id} not found")))?
    };

    // Down first, ignore errors
    let _ = docker::compose::down_stack(&name, &state.config).await;
    let _ = docker::compose::remove_stack(&name, &state.config).await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute("DELETE FROM services WHERE id = ?1", [&id])?;

    Ok(Json(serde_json::json!({"ok": true, "action": "removed"})))
}
