//! Account tab endpoints: profile, password change, session management.
//!
//! Single-admin Pier (no teams yet) — all handlers scope to the current
//! `AuthUser.id`, so each admin only sees/manages their own data.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::password;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// GET /api/v1/account/me — profile + last-login + session count.
pub async fn me(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (email, created_at): (String, String) = db.query_row(
        "SELECT email, created_at FROM users WHERE id = ?1",
        [&user.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let last_login: Option<String> = db
        .query_row(
            "SELECT MAX(created_at) FROM sessions WHERE user_id = ?1",
            [&user.id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();

    let session_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE user_id = ?1 AND expires_at > datetime('now')",
        [&user.id],
        |row| row.get(0),
    )?;

    Ok(Json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "email": email,
        "role": user.role,
        "created_at": created_at,
        "last_login_at": last_login,
        "session_count": session_count,
    })))
}

/// PUT /api/v1/account/password — verify current, set new, invalidate other sessions.
pub async fn change_password(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (username, email, current_hash): (String, String, String) = db.query_row(
        "SELECT username, email, password FROM users WHERE id = ?1",
        [&user.id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;

    // Same policy as the initial setup form — see `auth::password::
    // validate_password_strength`. Feeding username/email as `user_inputs`
    // lets zxcvbn penalise passwords derived from those fields.
    password::validate_password_strength(&body.new_password, &[&username, &email])
        .map_err(AppError::BadRequest)?;

    if !password::verify_password(&body.current_password, &current_hash)? {
        return Err(AppError::Unauthorized);
    }

    // Block "rotating" to the same password — defeats the point of a change.
    if password::verify_password(&body.new_password, &current_hash)? {
        return Err(AppError::BadRequest(
            "New password must differ from current".into(),
        ));
    }

    let new_hash = password::hash_password(&body.new_password)?;
    db.execute(
        "UPDATE users SET password = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![new_hash, user.id],
    )?;

    // Invalidate sessions other than the caller's — forces re-login everywhere else.
    let revoked = db.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND id != ?2",
        rusqlite::params![user.id, user.session_id],
    )?;

    tracing::info!(
        "Password changed for user {} ({} other sessions revoked)",
        user.username,
        revoked
    );

    Ok(Json(serde_json::json!({
        "ok": true,
        "revoked_sessions": revoked,
    })))
}

/// GET /api/v1/account/sessions — list the caller's active sessions.
pub async fn list_sessions(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, ip_address, user_agent, created_at, expires_at
         FROM sessions
         WHERE user_id = ?1 AND expires_at > datetime('now')
         ORDER BY created_at DESC",
    )?;

    let sessions: Vec<serde_json::Value> = stmt
        .query_map([&user.id], |row| {
            let id: String = row.get(0)?;
            let is_current = id == user.session_id;
            Ok(serde_json::json!({
                "id": id,
                "ip_address": row.get::<_, Option<String>>(1)?,
                "user_agent": row.get::<_, Option<String>>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "expires_at": row.get::<_, String>(4)?,
                "is_current": is_current,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(sessions))
}

/// DELETE /api/v1/account/sessions/:id — revoke one session (not the current).
pub async fn revoke_session(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    if id == user.session_id {
        return Err(AppError::BadRequest(
            "Cannot revoke your current session — use /logout instead".into(),
        ));
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "DELETE FROM sessions WHERE id = ?1 AND user_id = ?2",
        rusqlite::params![id, user.id],
    )?;

    if rows == 0 {
        return Err(AppError::NotFound(format!("Session {id} not found")));
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/v1/account/sessions/revoke-others — revoke all but the caller's.
pub async fn revoke_other_sessions(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND id != ?2",
        rusqlite::params![user.id, user.session_id],
    )?;

    Ok(Json(serde_json::json!({ "ok": true, "revoked": rows })))
}
