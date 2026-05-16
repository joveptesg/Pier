//! Public invitation acceptance endpoints.
//!
//! Routes here are *not* behind `require_auth` — the recipient of an
//! invitation link is by definition anonymous until they accept it.
//! Token verification happens in-handler via sha256 lookup against
//! `user_invitations.invite_token_hash`. Each token is single-use; on
//! accept we mark the row and create the corresponding `users` entry.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::audit::{self, AuthEvent};
use crate::auth::password;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct AcceptRequest {
    pub username: String,
    pub password: String,
}

/// GET /api/v1/invitations/{token} — public lookup. Returns 404 for unknown,
/// expired, or already-accepted invites so we don't leak which case it is.
pub async fn get(
    State(state): State<SharedState>,
    Path(token): Path<String>,
) -> AppResult<impl IntoResponse> {
    let token_hash = sha256_hex(&token);
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT email, default_global_role, expires_at
             FROM user_invitations
             WHERE invite_token_hash = ?1
               AND accepted_at IS NULL
               AND datetime(expires_at) > datetime('now')",
            [&token_hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (email, role, expires_at) =
        row.ok_or_else(|| AppError::NotFound("invitation not found".into()))?;
    Ok(Json(serde_json::json!({
        "email": email,
        "default_global_role": role,
        "expires_at": expires_at,
    })))
}

/// POST /api/v1/invitations/{token}/accept — redeem the invite, create the user.
///
/// Single-use: the moment we insert the user row we mark the invitation as
/// accepted with the new user's ID, so a second POST with the same token
/// fails the "still pending" check above.
pub async fn accept(
    State(state): State<SharedState>,
    Path(token): Path<String>,
    Json(body): Json<AcceptRequest>,
) -> AppResult<impl IntoResponse> {
    let token_hash = sha256_hex(&token);
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(AppError::BadRequest("username required".into()));
    }

    let new_user_id = uuid::Uuid::new_v4().to_string();
    let pw_hash;
    let email;
    let role;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let row: Option<(String, String, String)> = db
            .query_row(
                "SELECT id, email, default_global_role
                 FROM user_invitations
                 WHERE invite_token_hash = ?1
                   AND accepted_at IS NULL
                   AND datetime(expires_at) > datetime('now')",
                [&token_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (invite_id, email_v, role_v) =
            row.ok_or_else(|| AppError::NotFound("invitation not found".into()))?;

        password::validate_password_strength(&body.password, &[&username, &email_v])
            .map_err(AppError::BadRequest)?;
        pw_hash = password::hash_password(&body.password)
            .map_err(|e| anyhow::anyhow!("hash password: {e}"))?;
        email = email_v;
        role = role_v;

        let tx_now = chrono::Utc::now().to_rfc3339();
        // We don't have a tx wrapper handy; do the two writes inline, the
        // unique index on `invite_token_hash` + `accepted_at IS NULL` lookup
        // earlier in the same lock window keeps this safe from concurrent
        // double-accepts.
        db.execute(
            "INSERT INTO users
                (id, username, email, password, role, global_role,
                 is_active, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'user', ?5, 1, ?6, ?6)",
            params![new_user_id, username, email, pw_hash, role, tx_now],
        )?;
        db.execute(
            "UPDATE user_invitations
                SET accepted_at = ?1,
                    accepted_user_id = ?2
              WHERE id = ?3",
            params![tx_now, new_user_id, invite_id],
        )?;
    }

    audit::log(
        &state,
        AuthEvent::UserInviteAccepted,
        Some(&new_user_id),
        None,
        None,
        Some(serde_json::json!({
            "email": email,
            "global_role": role,
        })),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "user_id": new_user_id,
    })))
}

fn sha256_hex(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}
