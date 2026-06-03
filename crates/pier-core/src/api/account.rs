//! Account tab endpoints: profile, password change, session management.
//!
//! Single-admin Pier (no teams yet) — all handlers scope to the current
//! `AuthUser.id`, so each admin only sees/manages their own data.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{HeaderMap, SET_COOKIE, USER_AGENT};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::auth::audit::{self, AuthEvent};
use crate::auth::middleware::AuthUser;
use crate::auth::password;
use crate::auth::totp;
use crate::crypto;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

fn ip_of(addr: SocketAddr) -> Option<std::net::IpAddr> {
    Some(addr.ip())
}
fn ua_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(USER_AGENT).and_then(|v| v.to_str().ok())
}

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
        "global_role": user.global_role.as_str(),
        "is_peer": user.is_peer,
        "created_at": created_at,
        "last_login_at": last_login,
        "session_count": session_count,
    })))
}

/// PUT /api/v1/account/password — verify current, set new, invalidate other sessions.
pub async fn change_password(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<impl IntoResponse> {
    let ip = ip_of(addr);
    let ua = ua_of(&headers).map(|s| s.to_string());

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

    // Privilege change → rotate. First invalidate the caller's OTHER sessions
    // (re-login everywhere else)...
    let revoked = db.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND id != ?2",
        rusqlite::params![user.id, user.session_id],
    )?;
    // ...then drop the caller's CURRENT session row so we re-mint a fresh id
    // below: a leaked/old session id must not survive a password change. An
    // empty session_id means a token-authenticated caller — nothing to delete.
    if !user.session_id.is_empty() {
        db.execute("DELETE FROM sessions WHERE id = ?1", [&user.session_id])?;
    }

    drop(db);

    // Mint a fresh session for the caller (new random id). Must run AFTER
    // dropping the DB lock — `issue_session` takes the lock itself.
    let cookie = crate::auth::handlers::issue_session(&state, &user.id, ip, ua.as_deref())?;

    audit::log(
        &state,
        AuthEvent::PasswordChange,
        Some(&user.id),
        ip,
        ua.as_deref(),
        Some(serde_json::json!({"revoked_sessions": revoked})),
    );
    tracing::info!(
        "Password changed for user {} (session rotated, {} other sessions revoked)",
        user.username,
        revoked
    );

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({
            "ok": true,
            "revoked_sessions": revoked,
        })),
    ))
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
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    if id == user.session_id {
        return Err(AppError::BadRequest(
            "Cannot revoke your current session — use /logout instead".into(),
        ));
    }

    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        db.execute(
            "DELETE FROM sessions WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user.id],
        )?
    };

    if rows == 0 {
        return Err(AppError::NotFound(format!("Session {id} not found")));
    }

    audit::log(
        &state,
        AuthEvent::SessionRevoked,
        Some(&user.id),
        ip_of(addr),
        ua_of(&headers),
        Some(serde_json::json!({"session_id": id})),
    );

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/v1/account/sessions/revoke-others — revoke all but the caller's.
pub async fn revoke_other_sessions(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        db.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND id != ?2",
            rusqlite::params![user.id, user.session_id],
        )?
    };

    audit::log(
        &state,
        AuthEvent::SessionRevokedAll,
        Some(&user.id),
        ip_of(addr),
        ua_of(&headers),
        Some(serde_json::json!({"revoked": rows})),
    );

    Ok(Json(serde_json::json!({ "ok": true, "revoked": rows })))
}

// ─── 2FA (TOTP) ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TwoFaVerifyRequest {
    /// Base32 secret echoed back from /2fa/setup. We carry it through the
    /// client instead of stashing in a server-side scratch table to keep the
    /// "secret never persisted until verified" property simple.
    pub secret: String,
    pub code: String,
    /// Hashed recovery codes (returned alongside the secret in /2fa/setup),
    /// echoed back so we persist exactly what the user was shown.
    pub recovery_hashes: Vec<String>,
}

#[derive(Deserialize)]
pub struct TwoFaDisableRequest {
    /// Current 6-digit TOTP OR a one-shot recovery code (XXXX-XXXX-XXXX).
    /// Required so a hijacked session can't trivially weaken 2FA.
    pub code: String,
}

/// GET /api/v1/account/2fa — current enrollment state for the caller.
pub async fn two_fa_status(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let (enabled_at, recovery_json): (Option<String>, Option<String>) = db.query_row(
        "SELECT totp_enabled_at, totp_recovery_codes FROM users WHERE id = ?1",
        [&user.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let recovery_count: usize = recovery_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .map(|v| v.len())
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "enabled": enabled_at.is_some(),
        "enabled_at": enabled_at,
        "recovery_codes_remaining": recovery_count,
    })))
}

/// POST /api/v1/account/2fa/setup — generate a fresh secret + recovery codes.
/// Nothing is persisted yet; the client must finish the round-trip via /verify.
pub async fn two_fa_setup(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    // Refuse to re-issue if 2FA is already active — disable first, then re-enroll.
    let already_enabled: bool = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT totp_secret IS NOT NULL FROM users WHERE id = ?1",
            [&user.id],
            |row| row.get::<_, i64>(0),
        )? == 1
    };
    if already_enabled {
        return Err(AppError::Conflict(
            "2FA already enabled — disable first to re-enroll".into(),
        ));
    }

    let secret = totp::generate_secret();
    let otpauth = totp::otpauth_url(&user.username, &secret)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("otpauth url: {e}")))?;
    let qr_svg = totp::qr_svg(&otpauth)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("qr render: {e}")))?;
    let recovery_codes = totp::generate_recovery_codes(10);
    let recovery_hashes: Vec<String> = recovery_codes
        .iter()
        .map(|c| totp::hash_recovery_code(c))
        .collect();

    Ok(Json(serde_json::json!({
        "secret": secret,
        "otpauth_url": otpauth,
        "qr_svg": qr_svg,
        "recovery_codes": recovery_codes,
        "recovery_hashes": recovery_hashes,
    })))
}

/// POST /api/v1/account/2fa/verify — accept the first TOTP code and commit
/// the secret + recovery hashes to the DB. Failure leaves the user with 2FA off.
pub async fn two_fa_verify(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<TwoFaVerifyRequest>,
) -> AppResult<impl IntoResponse> {
    if !totp::check(&body.secret, &user.username, body.code.trim())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("totp check: {e}")))?
    {
        return Err(AppError::BadRequest(
            "Code did not match — check the time on your device and try again".into(),
        ));
    }

    let key = crypto::get_secret_key();
    let enc = crypto::encrypt(&body.secret, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt secret: {e}")))?;
    let recovery_json = serde_json::to_string(&body.recovery_hashes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize recovery: {e}")))?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE users
             SET totp_secret = ?1, totp_recovery_codes = ?2, totp_enabled_at = datetime('now')
             WHERE id = ?3",
            rusqlite::params![enc, recovery_json, user.id],
        )?;
    }

    audit::log(
        &state,
        AuthEvent::TwoFaEnabled,
        Some(&user.id),
        ip_of(addr),
        ua_of(&headers),
        None,
    );

    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/account/2fa/disable — NULL the 2FA columns. Requires a valid
/// TOTP code or recovery code to prevent a stolen session from weakening 2FA.
pub async fn two_fa_disable(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<TwoFaDisableRequest>,
) -> AppResult<impl IntoResponse> {
    let (totp_secret_enc, recovery_json): (Option<String>, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT totp_secret, totp_recovery_codes FROM users WHERE id = ?1",
            [&user.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
    };
    let Some(enc) = totp_secret_enc else {
        return Err(AppError::BadRequest("2FA is not enabled".into()));
    };

    let key = crypto::get_secret_key();
    let secret = crypto::decrypt(&enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt: {e}")))?;

    let code = body.code.trim();
    let ok_totp = totp::check(&secret, &user.username, code).unwrap_or(false);
    let ok_recovery = !ok_totp
        && recovery_json
            .as_deref()
            .and_then(|j| totp::consume_recovery_code(j, code))
            .is_some();

    if !ok_totp && !ok_recovery {
        return Err(AppError::Unauthorized);
    }

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE users
             SET totp_secret = NULL, totp_recovery_codes = NULL, totp_enabled_at = NULL
             WHERE id = ?1",
            [&user.id],
        )?;
    }

    audit::log(
        &state,
        AuthEvent::TwoFaDisabled,
        Some(&user.id),
        ip_of(addr),
        ua_of(&headers),
        None,
    );

    Ok(Json(serde_json::json!({"ok": true})))
}
