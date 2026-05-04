//! Settings for the embedded npm registry.
//!
//! Today this only exposes `s3_storage_id` — which row of `s3_storages`
//! to mirror tarballs into. Empty/null = no cold-tier mirroring.
//!
//! Stored in the generic `settings` key/value table under the
//! `registry.s3_storage_id` key so we don't churn migrations every time
//! a new toggle is added.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;

use crate::error::AppResult;
use crate::state::SharedState;

const KEY_S3_STORAGE_ID: &str = "registry.s3_storage_id";

#[derive(Deserialize)]
pub struct UpdateRequest {
    pub s3_storage_id: Option<String>,
}

/// `GET /api/v1/registry/settings`.
pub async fn get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let s3_id: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [KEY_S3_STORAGE_ID],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|s| !s.is_empty());

    Ok(Json(serde_json::json!({
        "s3_storage_id": s3_id,
    })))
}

/// `PUT /api/v1/registry/settings`. Empty/null `s3_storage_id` clears it,
/// disabling cold-tier mirroring. Validates the id actually points at an
/// existing s3_storages row before saving.
pub async fn update(
    State(state): State<SharedState>,
    Json(body): Json<UpdateRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let normalized = body
        .s3_storage_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    if let Some(id) = normalized {
        let exists: bool = db
            .query_row("SELECT 1 FROM s3_storages WHERE id = ?1", [id], |_| {
                Ok(true)
            })
            .unwrap_or(false);
        if !exists {
            return Err(crate::error::AppError::BadRequest(format!(
                "s3 storage '{id}' not found"
            )));
        }
    }

    let value = normalized.unwrap_or("");
    db.execute(
        "INSERT INTO settings (key, value, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        params![KEY_S3_STORAGE_ID, value],
    )?;

    Ok(Json(serde_json::json!({ "ok": true })))
}
