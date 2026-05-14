//! Security settings: account-wide preferences that gate destructive actions.
//!
//! Currently exposes a single toggle, `security.require_password_on_delete`,
//! which controls whether project/service/database delete handlers demand
//! re-entering the current user's password. Stored in the global `settings`
//! key/value table so the preference applies to all admins (Pier is
//! single-admin in practice — see `account.rs` module doc).

use axum::response::IntoResponse;
use axum::{extract::State, Json};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::auth::password;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

const KEY_REQUIRE_PW_ON_DELETE: &str = "security.require_password_on_delete";

/// Shared request body for any destructive endpoint that may be password-gated.
/// `password` is optional so the field can be omitted when the toggle is OFF.
#[derive(Deserialize)]
pub struct DeleteRequest {
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SecuritySettings {
    pub require_password_on_delete: bool,
}

/// Read the toggle from the `settings` table. Missing row = default ON.
pub fn require_password_on_delete(db: &rusqlite::Connection) -> AppResult<bool> {
    let val: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [KEY_REQUIRE_PW_ON_DELETE],
            |row| row.get(0),
        )
        .optional()?;
    Ok(val.map(|v| v != "false").unwrap_or(true))
}

/// Gate that destructive handlers call before touching anything. When the
/// toggle is OFF, returns Ok immediately. When ON, demands a non-empty
/// `provided` and verifies it against `users.password` (argon2).
///
/// Peer-token requests (no real session-bound user; `user.id` starts with
/// `peer:`) bypass the check — they're authenticated by a separate token.
pub fn verify_delete_password(
    db: &rusqlite::Connection,
    user_id: &str,
    provided: Option<&str>,
) -> AppResult<()> {
    if user_id.starts_with("peer:") {
        return Ok(());
    }
    if !require_password_on_delete(db)? {
        return Ok(());
    }
    let provided = provided.unwrap_or("").trim();
    if provided.is_empty() {
        return Err(AppError::Unauthorized);
    }
    let hash: String = db
        .query_row(
            "SELECT password FROM users WHERE id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .map_err(|_| AppError::Unauthorized)?;
    if !password::verify_password(provided, &hash)? {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

/// GET /api/v1/security/settings
pub async fn get_settings(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    Ok(Json(SecuritySettings {
        require_password_on_delete: require_password_on_delete(&db)?,
    }))
}

/// PUT /api/v1/security/settings
pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<SecuritySettings>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let val = if body.require_password_on_delete {
        "true"
    } else {
        "false"
    };
    db.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        rusqlite::params![KEY_REQUIRE_PW_ON_DELETE, val],
    )?;
    Ok(Json(SecuritySettings {
        require_password_on_delete: body.require_password_on_delete,
    }))
}
