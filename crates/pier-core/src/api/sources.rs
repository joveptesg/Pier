use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateSourceRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
    #[serde(default)]
    pub token: String,
    // GitHub App fields
    pub app_id: Option<String>,
    pub installation_id: Option<i64>,
    pub private_key: Option<String>,
}

/// GET /api/v1/sources
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, source_type, base_url, created_at
         FROM git_sources WHERE is_active = 1 ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "url": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/sources
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateSourceRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.url.trim().is_empty() {
        return Err(AppError::BadRequest("Name and URL are required".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = if body.token.is_empty() {
        None
    } else {
        Some(body.token.as_str())
    };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO git_sources (id, name, source_type, base_url, access_token, app_id, installation_id, private_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.source_type,
            body.url.trim(),
            token,
            body.app_id,
            body.installation_id,
            body.private_key
        ],
    )?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// DELETE /api/v1/sources/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM git_sources WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Source {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/v1/sources/{id}/repos
pub async fn list_repos(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (source_type, base_url, access_token, app_id, installation_id, private_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT source_type, base_url, access_token, app_id, installation_id, private_key FROM git_sources WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Source {id} not found")))?
    };

    let repos = if source_type == "github-app" {
        let app_id = app_id.ok_or_else(|| AppError::BadRequest("Missing app_id".into()))?;
        let inst_id = installation_id
            .ok_or_else(|| AppError::BadRequest("Missing installation_id".into()))?;
        let pk = private_key.ok_or_else(|| AppError::BadRequest("Missing private_key".into()))?;
        crate::git::github_app::list_repos(&app_id, inst_id, &pk)
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to fetch repos: {e}")))?
    } else {
        crate::git::list_repos(&source_type, &base_url, access_token.as_deref())
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to fetch repos: {e}")))?
    };

    Ok(Json(serde_json::json!(repos)))
}
