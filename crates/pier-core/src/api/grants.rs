//! External access — tokens that let another pier-core control this one.
//!
//! This is the **incoming** half of federation. The **outgoing** half (who
//! this core controls) lives in [`servers`] under `kind='peer'`.
//!
//! The `/api/v1/peers/probe` endpoint is the entry point a remote core hits
//! to verify its grant is valid; the auth middleware does the actual
//! `peer_grants` lookup and injects an admin-equivalent `AuthUser`.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/peers/probe — handshake endpoint for remote cores.
/// Auth (X-Pier-Peer-Token) is done by the middleware; if we get here, it's valid.
pub async fn probe(axum::Extension(user): axum::Extension<AuthUser>) -> impl IntoResponse {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "role": user.role,
        "global_role": user.global_role.as_str(),
        "principal": user.username,
        // Advertises that this core implements the core↔core mesh protocol
        // (/peers/mesh/describe|propose|teardown), so an initiator can refuse
        // to pair against an older peer that would 404 those routes.
        "supports_mesh": true,
    }))
}

#[derive(Deserialize)]
pub struct CreateGrantRequest {
    pub name: String,
}

/// GET /api/v1/grants — list grants this core exposes.
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, is_active, last_used_at, last_used_ip, created_at
         FROM peer_grants ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "is_active": row.get::<_, bool>(2)?,
                "last_used_at": row.get::<_, Option<String>>(3)?,
                "last_used_ip": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/grants — create a new grant and reveal its token once.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateGrantRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.grants.name_required",
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = catalog::generate_password(48);

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO peer_grants (id, name, token) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, name, token],
    )?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "name": name,
        "token": token,
    })))
}

/// DELETE /api/v1/grants/{id}
pub async fn revoke(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM peer_grants WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(crate::i18n::te_args(
            "errors.grants.grant_not_found",
            &[("v", &id)],
        )));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}
