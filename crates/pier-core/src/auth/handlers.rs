use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Json};
use serde::Deserialize;

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
    if body.username.trim().is_empty() || body.password.len() < 8 {
        return Err(AppError::BadRequest(
            "Username required, password must be at least 8 characters".into(),
        ));
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Check no users exist
    let count: u32 = db.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
    if count > 0 {
        return Err(AppError::Conflict("Setup already completed".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let hash = password::hash_password(&body.password)?;

    db.execute(
        "INSERT INTO users (id, username, email, password, role) VALUES (?1, ?2, ?3, ?4, 'admin')",
        rusqlite::params![id, body.username.trim(), body.email.trim(), hash],
    )?;

    tracing::info!("Admin user '{}' created", body.username.trim());
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

    // Find user by username
    let user_result = db.query_row(
        "SELECT id, password FROM users WHERE username = ?1 AND is_active = 1",
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

    // Build Set-Cookie header
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        state.config.session_cookie,
        session_id,
        ttl * 3600,
    );

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    ))
}

/// POST /api/v1/auth/logout — Destroy current session.
pub async fn logout(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Clear cookie (session cleanup happens via middleware check)
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        state.config.session_cookie,
    );

    Ok((
        [(SET_COOKIE, cookie)],
        Json(serde_json::json!({"ok": true})),
    ))
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
