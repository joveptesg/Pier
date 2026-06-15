//! User management API (Admin / Owner only).
//!
//! Endpoints under `/api/v1/users/**`. Authorisation is enforced by the
//! `require_global_admin` router-layer in `api/mod.rs`; the Owner-only
//! `change_role` sits on its own `require_global_owner` layer.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::audit::{self, AuthEvent};
use crate::auth::middleware::AuthUser;
use crate::auth::rbac::GlobalRole;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct InviteRequest {
    pub email: String,
    /// `owner` is rejected — Owner role is granted only via `change_role`
    /// once the user already exists, and only by an existing Owner.
    pub global_role: Option<String>,
    /// Time-to-live in hours. Clamped to [1, 168] (1 hour … 7 days). Defaults
    /// to 48h if omitted.
    pub ttl_hours: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub username: Option<String>,
    pub email: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize)]
pub struct ChangeRoleRequest {
    pub global_role: String,
}

/// GET /api/v1/users — list every user.
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, username, email, role, global_role, is_active, created_at,
                totp_enabled_at IS NOT NULL AS has_2fa
         FROM users ORDER BY created_at ASC",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id":           row.get::<_, String>(0)?,
                "username":     row.get::<_, String>(1)?,
                "email":        row.get::<_, String>(2)?,
                "role":         row.get::<_, String>(3)?,
                "global_role":  row.get::<_, String>(4)?,
                "is_active":    row.get::<_, bool>(5)?,
                "created_at":   row.get::<_, String>(6)?,
                "has_2fa":      row.get::<_, bool>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

/// POST /api/v1/users/invite — issue a one-time invitation link.
///
/// Returns the plaintext token in the response body exactly once. The DB
/// only stores the sha256 hash; if the inviter loses the link they must
/// revoke the invite and create a new one.
pub async fn invite(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Json(body): Json<InviteRequest>,
) -> AppResult<impl IntoResponse> {
    let email = body.email.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.users.valid_email_required",
        )));
    }
    let default_global_role = match body.global_role.as_deref() {
        None | Some("user") => "user",
        Some("admin") => "admin",
        Some("owner") => {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.users.owner_role_not_via_invitation",
            )))
        }
        Some(other) => {
            return Err(AppError::BadRequest(crate::i18n::te_args(
                "errors.users.unknown_role",
                &[("name", other)],
            )))
        }
    };
    if default_global_role == "admin" && actor.global_role != GlobalRole::Owner {
        return Err(AppError::Forbidden(crate::i18n::te(
            "errors.users.only_owner_can_invite_admins",
        )));
    }
    let ttl_hours = body.ttl_hours.unwrap_or(48).clamp(1, 168);

    let token_bytes: [u8; 24] = rand::random();
    let plaintext = format!("pier_invite_{}", hex::encode(token_bytes));
    let token_hash = sha256_hex(&plaintext);
    let id = uuid::Uuid::new_v4().to_string();

    let actor_id = actor.id.clone();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        // Reject duplicate active invitations for the same email — the
        // inviter should explicitly revoke the old one or the recipient
        // should accept it first.
        let active: Option<String> = db
            .query_row(
                "SELECT id FROM user_invitations
                 WHERE email = ?1
                   AND accepted_at IS NULL
                   AND datetime(expires_at) > datetime('now')",
                [&email],
                |row| row.get(0),
            )
            .optional()?;
        if active.is_some() {
            return Err(AppError::Conflict(crate::i18n::te(
                "errors.users.active_invitation_exists",
            )));
        }
        db.execute(
            "INSERT INTO user_invitations
                (id, email, invite_token_hash, default_global_role,
                 invited_by, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     datetime('now', ?6 || ' hours'))",
            params![
                id,
                email,
                token_hash,
                default_global_role,
                actor_id,
                ttl_hours.to_string(),
            ],
        )?;
    }

    audit::log(
        &state,
        AuthEvent::UserInvited,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "invite_id": id,
            "email": email,
            "default_global_role": default_global_role,
            "ttl_hours": ttl_hours,
        })),
    );

    Ok(Json(serde_json::json!({
        "id": id,
        "email": email,
        "default_global_role": default_global_role,
        "token": plaintext,
        "invite_url": format!("/invitations/{plaintext}"),
        "ttl_hours": ttl_hours,
    })))
}

/// PUT /api/v1/users/{id} — update profile fields. Owner-Admin can edit
/// everyone; Admin can edit anyone but Owner; users can edit themselves
/// via /api/v1/account/* (this endpoint refuses self-edits to keep that
/// path canonical).
pub async fn update(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path(target_id): Path<String>,
    Json(body): Json<UpdateUserRequest>,
) -> AppResult<impl IntoResponse> {
    if target_id == actor.id {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.users.use_account_endpoints",
        )));
    }
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let target_role: String = db
        .query_row(
            "SELECT global_role FROM users WHERE id = ?1",
            [&target_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound(crate::i18n::te("errors.users.user_not_found")))?;
    let target_role = GlobalRole::parse(&target_role).unwrap_or(GlobalRole::User);
    if target_role == GlobalRole::Owner && actor.global_role != GlobalRole::Owner {
        return Err(AppError::Forbidden(crate::i18n::te(
            "errors.users.only_owner_can_edit_owner",
        )));
    }

    if let Some(username) = body.username {
        db.execute(
            "UPDATE users SET username = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![username, target_id],
        )?;
    }
    if let Some(email) = body.email {
        db.execute(
            "UPDATE users SET email = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![email.to_ascii_lowercase(), target_id],
        )?;
    }
    if let Some(active) = body.is_active {
        db.execute(
            "UPDATE users SET is_active = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![active as i64, target_id],
        )?;
        if !active {
            // Offboarding: a deactivated user must lose ALL access immediately,
            // not merely be blocked at validation time. Delete their sessions and
            // revoke their API tokens so the cut-off is real (and visible in the
            // UI/audit), not just an `is_active = 0` guard everyone must remember.
            let sessions = db.execute("DELETE FROM sessions WHERE user_id = ?1", [&target_id])?;
            let tokens = db.execute(
                "UPDATE api_tokens SET revoked_at = ?2 \
                 WHERE user_id = ?1 AND revoked_at IS NULL",
                params![target_id, chrono::Utc::now().timestamp()],
            )?;
            tracing::info!(
                user = %target_id,
                sessions,
                tokens,
                "deactivated user; revoked active sessions and API tokens"
            );
        }
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/users/{id} — hard delete (cascades sessions, memberships).
///
/// Refused for self-delete and for last-Owner. Owner can delete any user;
/// Admin cannot delete another Admin or the Owner.
pub async fn remove(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path(target_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    if target_id == actor.id {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.users.cannot_delete_yourself",
        )));
    }
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let target_role_s: String = db
        .query_row(
            "SELECT global_role FROM users WHERE id = ?1",
            [&target_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound(crate::i18n::te("errors.users.user_not_found")))?;
    let target_role = GlobalRole::parse(&target_role_s).unwrap_or(GlobalRole::User);

    if target_role == GlobalRole::Owner {
        let owners: i64 = db.query_row(
            "SELECT COUNT(*) FROM users WHERE global_role = 'owner' AND is_active = 1",
            [],
            |r| r.get(0),
        )?;
        if owners <= 1 {
            return Err(AppError::Conflict(crate::i18n::te(
                "errors.users.cannot_delete_last_owner",
            )));
        }
        if actor.global_role != GlobalRole::Owner {
            return Err(AppError::Forbidden(crate::i18n::te(
                "errors.users.only_owner_can_delete_owner",
            )));
        }
    } else if target_role == GlobalRole::Admin && actor.global_role != GlobalRole::Owner {
        return Err(AppError::Forbidden(crate::i18n::te(
            "errors.users.only_owner_can_delete_admins",
        )));
    }

    db.execute("DELETE FROM users WHERE id = ?1", [&target_id])?;
    drop(db);

    audit::log(
        &state,
        AuthEvent::UserDeleted,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "target_id": target_id,
            "target_role": target_role.as_str(),
        })),
    );
    Ok(Json(serde_json::json!({"ok": true})))
}

/// PUT /api/v1/users/{id}/role — change global role. Owner-only. Enforces
/// "≥1 active Owner" so the last Owner cannot demote themselves into a
/// recovery-locked state.
pub async fn change_role(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path(target_id): Path<String>,
    Json(body): Json<ChangeRoleRequest>,
) -> AppResult<impl IntoResponse> {
    let new_role = GlobalRole::parse(&body.global_role).ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.users.unknown_role",
            &[("name", &body.global_role)],
        ))
    })?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let current_s: String = db
        .query_row(
            "SELECT global_role FROM users WHERE id = ?1",
            [&target_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound(crate::i18n::te("errors.users.user_not_found")))?;
    let current = GlobalRole::parse(&current_s).unwrap_or(GlobalRole::User);
    if current == new_role {
        return Ok(Json(
            serde_json::json!({"ok": true, "unchanged": true, "global_role": new_role.as_str()}),
        ));
    }

    // Last-Owner check fires regardless of the new role: a single-Owner
    // demotion would leave the instance ownerless until someone hand-edits
    // SQLite.
    if current == GlobalRole::Owner {
        let owners: i64 = db.query_row(
            "SELECT COUNT(*) FROM users WHERE global_role = 'owner' AND is_active = 1",
            [],
            |r| r.get(0),
        )?;
        if owners <= 1 {
            return Err(AppError::Conflict(crate::i18n::te(
                "errors.users.cannot_demote_last_owner",
            )));
        }
    }

    db.execute(
        "UPDATE users SET global_role = ?1, updated_at = datetime('now') WHERE id = ?2",
        params![new_role.as_str(), target_id],
    )?;
    drop(db);

    audit::log(
        &state,
        AuthEvent::UserRoleChanged,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "target_id": target_id,
            "from": current.as_str(),
            "to": new_role.as_str(),
        })),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "global_role": new_role.as_str(),
    })))
}

/// sha256 hex helper — same encoding as `auth::api_token::hash` so we don't
/// pull in a second hash strategy.
fn sha256_hex(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}
