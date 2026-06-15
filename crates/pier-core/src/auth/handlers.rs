use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::header::{HeaderMap, SET_COOKIE, USER_AGENT};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;

use crate::crypto;
use crate::error::{AppError, AppResult};
use crate::i18n::te;
use crate::state::SharedState;

use super::audit::{self, AuthEvent};
use super::cookie::build_session_cookie;
use super::password;
use super::totp;

#[derive(Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub email: String,
    pub password: String,
    /// Bootstrap token printed by install.sh. Required when the server was
    /// launched with a token file present; ignored when it wasn't.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginTotpRequest {
    pub partial_token: String,
    pub code: String,
}

/// POST /api/v1/auth/setup — Create the first admin user.
/// Only works if no users exist in the database AND (when a setup token is
/// configured) the request supplies the matching `token`.
pub async fn setup(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<SetupRequest>,
) -> AppResult<impl IntoResponse> {
    let ip = Some(addr.ip());
    let ua = headers.get(USER_AGENT).and_then(|v| v.to_str().ok());

    // Token gate — when install.sh seeded a token, refuse setup without it.
    // We treat a missing/wrong token as 404-equivalent at the API layer
    // (BadRequest with a generic message) so we don't help probers distinguish
    // "wrong token" from "wrong field name".
    if state.setup_token.is_required() {
        let provided = body.token.as_deref().unwrap_or("");
        if !state.setup_token.matches(provided) {
            return Err(AppError::NotFound(te("errors.auth.setup_not_found")));
        }
    }

    let username = body.username.trim();
    let email = body.email.trim();

    if username.is_empty() {
        return Err(AppError::BadRequest(te("errors.auth.username_required")));
    }
    password::validate_password_strength(&body.password, &[username, email])
        .map_err(AppError::BadRequest)?;

    let id = uuid::Uuid::new_v4().to_string();
    let hash = password::hash_password(&body.password)?;

    let inserted = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        // Atomic guard against the "race for first admin" on a fresh VPS: between
        // a SELECT COUNT and an INSERT, two concurrent setup calls could both
        // succeed. This single statement inserts only when the users table is
        // still empty; SQLite serialises it under the database write lock.
        let n = db.execute(
            "INSERT INTO users (id, username, email, password, role, global_role)
             SELECT ?1, ?2, ?3, ?4, 'admin', 'owner'
             WHERE NOT EXISTS (SELECT 1 FROM users)",
            rusqlite::params![id, username, email, hash],
        )?;
        if n > 0 {
            // Seed `proxy.acme_email` from the admin's email so Let's Encrypt has a
            // valid contact on first deploy and the UI shows it pre-filled. Skip if
            // the operator already set it explicitly (`INSERT OR IGNORE`).
            db.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES ('proxy.acme_email', ?1)",
                [email],
            )?;
        }
        n
    };

    if inserted == 0 {
        return Err(AppError::Conflict(te(
            "errors.auth.setup_already_completed",
        )));
    }

    // Consume the bootstrap token AFTER the user has been persisted. If we
    // consumed first and the INSERT failed, the operator would be locked out
    // unless they re-ran install.sh.
    state.setup_token.consume();

    audit::log(&state, AuthEvent::Setup, Some(&id), ip, ua, None);
    tracing::info!("Admin user '{}' created", username);
    Ok(Json(
        serde_json::json!({"ok": true, "message": "Admin user created"}),
    ))
}

/// POST /api/v1/auth/login — Authenticate. When 2FA is enabled for the user
/// this returns a short-lived `partial_token` instead of a session cookie;
/// the client must follow up with /auth/login/2fa.
pub async fn login(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> AppResult<Response> {
    let ip = Some(addr.ip());
    let ua = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let lookup = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        // Find user by username OR email
        db.query_row(
            "SELECT id, password, totp_secret FROM users
             WHERE (username = ?1 OR email = ?1) AND is_active = 1",
            [&body.username],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
    };

    let (user_id, hash, totp_secret_enc) = match lookup {
        Ok(u) => u,
        Err(_) => {
            // Reason omitted from `details` for unknown_user: we don't want
            // the audit log itself to confirm whether a given username exists.
            audit::log(
                &state,
                AuthEvent::LoginFailure,
                None,
                ip,
                ua.as_deref(),
                Some(serde_json::json!({"reason": "unknown_user"})),
            );
            return Err(AppError::BadRequest(te("errors.auth.invalid_credentials")));
        }
    };

    if !password::verify_password(&body.password, &hash)? {
        audit::log(
            &state,
            AuthEvent::LoginFailure,
            Some(&user_id),
            ip,
            ua.as_deref(),
            Some(serde_json::json!({"reason": "wrong_password"})),
        );
        return Err(AppError::BadRequest(te("errors.auth.invalid_credentials")));
    }

    // Branch on whether 2FA is configured for this user.
    if totp_secret_enc.is_some() {
        let partial = state.partial_tokens.issue(user_id.clone(), ip);
        audit::log(
            &state,
            AuthEvent::LoginTotpRequired,
            Some(&user_id),
            ip,
            ua.as_deref(),
            None,
        );
        return Ok(Json(serde_json::json!({
            "ok": true,
            "requires_2fa": true,
            "partial_token": partial,
        }))
        .into_response());
    }

    let cookie = issue_session(&state, &user_id, ip, ua.as_deref())?;
    audit::log(
        &state,
        AuthEvent::LoginSuccess,
        Some(&user_id),
        ip,
        ua.as_deref(),
        None,
    );

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response())
}

/// POST /api/v1/auth/login/2fa — second step. Accepts either the current TOTP
/// code or a one-shot recovery code; sets the session cookie on success.
pub async fn login_2fa(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginTotpRequest>,
) -> AppResult<impl IntoResponse> {
    let ip = Some(addr.ip());
    let ua = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let Some(user_id) = state.partial_tokens.consume(&body.partial_token, ip) else {
        audit::log(
            &state,
            AuthEvent::LoginTotpFailure,
            None,
            ip,
            ua.as_deref(),
            Some(serde_json::json!({"reason": "expired_partial"})),
        );
        return Err(AppError::Unauthorized);
    };

    let (username, totp_secret_enc, recovery_json): (String, Option<String>, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT username, totp_secret, totp_recovery_codes
             FROM users WHERE id = ?1 AND is_active = 1",
            [&user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
    };

    let Some(enc) = totp_secret_enc else {
        // Race: 2FA disabled between password step and TOTP step. Fail safe.
        audit::log(
            &state,
            AuthEvent::LoginTotpFailure,
            Some(&user_id),
            ip,
            ua.as_deref(),
            Some(serde_json::json!({"reason": "totp_not_configured"})),
        );
        return Err(AppError::Unauthorized);
    };

    let key = crypto::get_secret_key();
    let secret = crypto::decrypt(&enc, &key).map_err(|e| anyhow::anyhow!("totp decrypt: {e}"))?;

    let code = body.code.trim();
    if totp::check(&secret, &username, code).unwrap_or(false) {
        let cookie = issue_session(&state, &user_id, ip, ua.as_deref())?;
        audit::log(
            &state,
            AuthEvent::LoginTotpSuccess,
            Some(&user_id),
            ip,
            ua.as_deref(),
            None,
        );
        return Ok((
            [(SET_COOKIE, cookie)],
            Json(serde_json::json!({"ok": true})),
        ));
    }

    // Not a TOTP code — try as recovery code.
    if let Some(json) = recovery_json {
        if let Some(remaining) = totp::consume_recovery_code(&json, code) {
            // Persist the shortened list so the same code can't be reused.
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "UPDATE users SET totp_recovery_codes = ?1 WHERE id = ?2",
                    rusqlite::params![remaining, user_id],
                )?;
            }
            let cookie = issue_session(&state, &user_id, ip, ua.as_deref())?;
            audit::log(
                &state,
                AuthEvent::LoginTotpSuccess,
                Some(&user_id),
                ip,
                ua.as_deref(),
                Some(serde_json::json!({"used": "recovery_code"})),
            );
            return Ok((
                [(SET_COOKIE, cookie)],
                Json(serde_json::json!({"ok": true, "via": "recovery_code"})),
            ));
        }
    }

    audit::log(
        &state,
        AuthEvent::LoginTotpFailure,
        Some(&user_id),
        ip,
        ua.as_deref(),
        Some(serde_json::json!({"reason": "wrong_code"})),
    );
    Err(AppError::Unauthorized)
}

/// POST /api/v1/auth/logout — Destroy current session.
pub async fn logout(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    user: Option<axum::Extension<super::middleware::AuthUser>>,
) -> AppResult<impl IntoResponse> {
    let ip = Some(addr.ip());
    let ua = headers.get(USER_AGENT).and_then(|v| v.to_str().ok());

    if let Some(axum::Extension(u)) = user {
        // Best-effort: delete the session row so the cookie can't be replayed
        // even if the client ignores Set-Cookie.
        if !u.session_id.is_empty() {
            if let Ok(db) = state.db.lock() {
                let _ = db.execute("DELETE FROM sessions WHERE id = ?1", [&u.session_id]);
            }
        }
        audit::log(&state, AuthEvent::Logout, Some(&u.id), ip, ua, None);
    }

    let cookie = build_session_cookie(&state, "", 0);
    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    ))
}

/// Insert a session row and return the cookie header value.
pub(crate) fn issue_session(
    state: &SharedState,
    user_id: &str,
    ip: Option<std::net::IpAddr>,
    ua: Option<&str>,
) -> AppResult<String> {
    let session_id = generate_session_id();
    let ttl = state.config.session_ttl_hours as i64;
    let ip_str = ip.map(|i| i.to_string());

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO sessions (id, user_id, ip_address, user_agent, expires_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now', '+' || ?5 || ' hours'))",
            rusqlite::params![session_id, user_id, ip_str, ua, ttl],
        )?;
    }

    Ok(build_session_cookie(state, &session_id, ttl * 3600))
}

/// GET /api/v1/auth/session — Return current user info.
pub async fn session_check(
    axum::Extension(user): axum::Extension<super::middleware::AuthUser>,
) -> AppResult<impl IntoResponse> {
    Ok(Json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
        "global_role": user.global_role.as_str(),
        "is_peer": user.is_peer,
    })))
}

/// Generate a cryptographically random session ID (32 bytes, hex-encoded).
fn generate_session_id() -> String {
    use rand::RngExt;
    let bytes: [u8; 32] = rand::rng().random();
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
