//! Peer-side management of `federation_tokens` rows — the tokens a
//! peer pier-core mints so a remote primary can drive write operations
//! against `/api/v1/agent/*` on this peer.
//!
//! Lives under the normal session auth (operator typing in their own
//! peer's UI). The actual federation surface this gates is mounted
//! separately at `/api/v1/agent/*` and uses
//! [`crate::auth::federation::require_federation`] — these two routes
//! never share auth, by design.
//!
//! Mirrors [`super::grants`] in shape but for the opposite federation
//! direction (peer→primary trust vs peer↔peer trust).

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::federation::{generate, hash};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    /// Operator-friendly identifier for the primary this token will
    /// authenticate. Shown verbatim on the peer's UI as
    /// "managed by <label>" later.
    pub label: String,
}

/// GET /api/v1/federation-tokens
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, token_prefix, label, is_active, created_at, last_used_at \
         FROM federation_tokens \
         ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "token_prefix": row.get::<_, String>(1)?,
                "label": row.get::<_, String>(2)?,
                "is_active": row.get::<_, i64>(3)? != 0,
                "created_at": row.get::<_, i64>(4)?,
                "last_used_at": row.get::<_, Option<i64>>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/federation-tokens
///
/// Mints a fresh federation token and returns its plaintext **exactly
/// once**. The operator copies it from this response into the
/// primary's "Connect to peer" form; afterwards the peer keeps only
/// the SHA-256 hash and the visible prefix.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateTokenRequest>,
) -> AppResult<impl IntoResponse> {
    let label = body.label.trim().to_string();
    if label.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.federation_tokens.label_required",
        )));
    }
    let issued = generate();
    let token_hash = hash(&issued.plaintext);
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO federation_tokens \
            (id, token_hash, token_prefix, label, is_active, created_at) \
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        rusqlite::params![id, token_hash, issued.prefix, label, now],
    )?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "label": label,
        "token_prefix": issued.prefix,
        // Plaintext — the operator copies this into the primary's
        // server form. Pier never shows it again.
        "token": issued.plaintext,
    })))
}

/// DELETE /api/v1/federation-tokens/{id} — revoke.
///
/// Sets `is_active = 0` rather than deleting the row, so primary
/// calls that used the token surface as 401 with a clean audit trail.
/// A future janitor can clean up rows that have been inactive for
/// long enough.
pub async fn revoke(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute(
        "UPDATE federation_tokens SET is_active = 0 WHERE id = ?1",
        [&id],
    )?;
    if rows == 0 {
        return Err(AppError::NotFound(crate::i18n::te_args(
            "errors.federation_tokens.token_not_found",
            &[("v", &id)],
        )));
    }
    Ok(Json(serde_json::json!({"ok": true, "action": "revoked"})))
}
