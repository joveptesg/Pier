use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Json};
use serde::Deserialize;

use crate::config::TlsMode;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

use super::password;

#[derive(Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// POST /api/v1/auth/setup — Create the first admin user.
/// Only works if no users exist in the database.
pub async fn setup(
    State(state): State<SharedState>,
    Json(body): Json<SetupRequest>,
) -> AppResult<impl IntoResponse> {
    let username = body.username.trim();
    let email = body.email.trim();

    if username.is_empty() {
        return Err(AppError::BadRequest("Username required".into()));
    }
    password::validate_password_strength(&body.password, &[username, email])
        .map_err(AppError::BadRequest)?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let id = uuid::Uuid::new_v4().to_string();
    let hash = password::hash_password(&body.password)?;

    // Atomic guard against the "race for first admin" on a fresh VPS: between
    // a SELECT COUNT and an INSERT, two concurrent setup calls could both
    // succeed. This single statement inserts only when the users table is
    // still empty; SQLite serialises it under the database write lock.
    let inserted = db.execute(
        "INSERT INTO users (id, username, email, password, role)
         SELECT ?1, ?2, ?3, ?4, 'admin'
         WHERE NOT EXISTS (SELECT 1 FROM users)",
        rusqlite::params![id, username, email, hash],
    )?;
    if inserted == 0 {
        return Err(AppError::Conflict("Setup already completed".into()));
    }

    // Seed `proxy.acme_email` from the admin's email so Let's Encrypt has a
    // valid contact on first deploy and the UI shows it pre-filled. Skip if
    // the operator already set it explicitly (`INSERT OR IGNORE`).
    db.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('proxy.acme_email', ?1)",
        [email],
    )?;

    tracing::info!("Admin user '{}' created", username);
    Ok(Json(
        serde_json::json!({"ok": true, "message": "Admin user created"}),
    ))
}

/// POST /api/v1/auth/login — Authenticate and create session.
pub async fn login(
    State(state): State<SharedState>,
    Json(body): Json<LoginRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Find user by username OR email
    let user_result = db.query_row(
        "SELECT id, password FROM users WHERE (username = ?1 OR email = ?1) AND is_active = 1",
        [&body.username],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    );

    let (user_id, hash) = match user_result {
        Ok(u) => u,
        Err(_) => return Err(AppError::BadRequest("Invalid credentials".into())),
    };

    // Verify password
    if !password::verify_password(&body.password, &hash)? {
        return Err(AppError::BadRequest("Invalid credentials".into()));
    }

    // Generate session token
    let session_id = generate_session_id();
    let ttl = state.config.session_ttl_hours as i64;

    db.execute(
        "INSERT INTO sessions (id, user_id, expires_at)
         VALUES (?1, ?2, datetime('now', '+' || ?3 || ' hours'))",
        rusqlite::params![session_id, user_id, ttl],
    )?;

    let cookie = build_session_cookie(&state, &session_id, ttl * 3600);

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    ))
}

/// POST /api/v1/auth/logout — Destroy current session.
pub async fn logout(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Clear cookie (session cleanup happens via middleware check)
    let cookie = build_session_cookie(&state, "", 0);

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    ))
}

/// Build a `Set-Cookie` header for the session.
///
/// `Secure` is set whenever TLS termination is in-process. We deliberately do
/// not set it when `tls_mode == Off` so that an operator who terminates TLS at
/// a separate reverse proxy and runs Pier on plain HTTP locally still gets a
/// working session cookie. `SameSite=Strict` is fine here because the only
/// legitimate path to the panel is the operator typing the URL — there are no
/// cross-origin flows we want to preserve.
fn build_session_cookie(state: &SharedState, value: &str, max_age_secs: i64) -> String {
    let secure = if state.config.tls_mode == TlsMode::Off {
        ""
    } else {
        "Secure; "
    };
    format!(
        "{}={}; Path=/; HttpOnly; {}SameSite=Strict; Max-Age={}",
        state.config.session_cookie, value, secure, max_age_secs,
    )
}

/// GET /api/v1/auth/session — Return current user info.
pub async fn session_check(
    axum::Extension(user): axum::Extension<super::middleware::AuthUser>,
) -> AppResult<impl IntoResponse> {
    Ok(Json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
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
