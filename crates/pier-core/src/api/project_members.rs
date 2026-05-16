//! Project membership management.
//!
//! All endpoints sit under `/api/v1/projects/{id}/members/**`. Authorisation
//! is two-tier: global Admin+ can act on any project; otherwise the caller
//! must be a Project Admin on the project in question.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

use crate::auth::audit::{self, AuthEvent};
use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_project_role, membership, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct AddMemberRequest {
    /// Either explicit user_id or an email; one is required. Email lookup
    /// is case-insensitive against `users.email`.
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub project_role: String,
}

#[derive(Deserialize)]
pub struct UpdateMemberRequest {
    pub project_role: String,
}

/// GET /api/v1/projects/{id}/members
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path(project_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    // Viewers can see the membership list too — knowing who else is on the
    // project doesn't leak more than the role chip already does.
    enforce_project_role(&actor, &project_id, ProjectRole::Viewer, &db)?;

    let mut stmt = db.prepare(
        "SELECT pm.id, pm.user_id, u.username, u.email, pm.project_role,
                pm.added_at, pm.added_by
         FROM project_members pm
         JOIN users u ON u.id = pm.user_id
         WHERE pm.project_id = ?1
         ORDER BY
            CASE pm.project_role
                WHEN 'admin'  THEN 0
                WHEN 'editor' THEN 1
                WHEN 'viewer' THEN 2
                ELSE 3
            END,
            pm.added_at ASC",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([&project_id], |r| {
            Ok(serde_json::json!({
                "id":           r.get::<_, String>(0)?,
                "user_id":      r.get::<_, String>(1)?,
                "username":     r.get::<_, String>(2)?,
                "email":        r.get::<_, String>(3)?,
                "project_role": r.get::<_, String>(4)?,
                "added_at":     r.get::<_, String>(5)?,
                "added_by":     r.get::<_, Option<String>>(6)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

/// POST /api/v1/projects/{id}/members — add a member.
pub async fn add(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path(project_id): Path<String>,
    Json(body): Json<AddMemberRequest>,
) -> AppResult<impl IntoResponse> {
    let project_role = ProjectRole::parse(&body.project_role).ok_or_else(|| {
        AppError::BadRequest(format!("unknown project_role: {}", body.project_role))
    })?;

    let resolved_user_id = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        enforce_project_role(&actor, &project_id, ProjectRole::Admin, &db)?;

        if let Some(uid) = body.user_id {
            // Validate the user exists.
            let exists: Option<String> = db
                .query_row("SELECT id FROM users WHERE id = ?1", [&uid], |r| r.get(0))
                .optional()?;
            exists.ok_or_else(|| AppError::NotFound("user not found".into()))?
        } else if let Some(email) = body.email {
            let email_lower = email.to_ascii_lowercase();
            db.query_row(
                "SELECT id FROM users WHERE lower(email) = ?1",
                [&email_lower],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| AppError::NotFound("user with that email not found".into()))?
        } else {
            return Err(AppError::BadRequest(
                "user_id or email is required".into(),
            ));
        }
    };

    let member_id = uuid::Uuid::new_v4().to_string();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let inserted = db.execute(
            "INSERT OR IGNORE INTO project_members
                (id, project_id, user_id, project_role, added_by)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                member_id,
                project_id,
                resolved_user_id,
                project_role.as_str(),
                actor.id,
            ],
        )?;
        if inserted == 0 {
            return Err(AppError::Conflict(
                "user is already a member of this project".into(),
            ));
        }
    }

    audit::log(
        &state,
        AuthEvent::ProjectMemberAdded,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "project_id": project_id,
            "user_id": resolved_user_id,
            "project_role": project_role.as_str(),
        })),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": member_id,
        "user_id": resolved_user_id,
        "project_role": project_role.as_str(),
    })))
}

/// PUT /api/v1/projects/{id}/members/{user_id} — change role.
///
/// Refuses any change that would leave the project with zero Project Admins.
pub async fn update_role(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path((project_id, target_user_id)): Path<(String, String)>,
    Json(body): Json<UpdateMemberRequest>,
) -> AppResult<impl IntoResponse> {
    let new_role = ProjectRole::parse(&body.project_role).ok_or_else(|| {
        AppError::BadRequest(format!("unknown project_role: {}", body.project_role))
    })?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    enforce_project_role(&actor, &project_id, ProjectRole::Admin, &db)?;

    let current_s: String = db
        .query_row(
            "SELECT project_role FROM project_members
             WHERE project_id = ?1 AND user_id = ?2",
            [&project_id, &target_user_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound("membership not found".into()))?;
    let current = ProjectRole::parse(&current_s).unwrap_or(ProjectRole::Viewer);

    if current == ProjectRole::Admin && new_role != ProjectRole::Admin {
        let admins = membership::count_admins(&db, &project_id)
            .map_err(|e| anyhow::anyhow!("count admins: {e}"))?;
        if admins <= 1 {
            return Err(AppError::Conflict(
                "cannot demote the last Project Admin".into(),
            ));
        }
    }

    db.execute(
        "UPDATE project_members SET project_role = ?1
         WHERE project_id = ?2 AND user_id = ?3",
        params![new_role.as_str(), project_id, target_user_id],
    )?;
    drop(db);

    audit::log(
        &state,
        AuthEvent::ProjectMemberRoleChanged,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "project_id": project_id,
            "user_id": target_user_id,
            "from": current.as_str(),
            "to": new_role.as_str(),
        })),
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "project_role": new_role.as_str(),
    })))
}

/// DELETE /api/v1/projects/{id}/members/{user_id}
pub async fn remove(
    State(state): State<SharedState>,
    axum::Extension(actor): axum::Extension<AuthUser>,
    Path((project_id, target_user_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    enforce_project_role(&actor, &project_id, ProjectRole::Admin, &db)?;

    let current_s: Option<String> = db
        .query_row(
            "SELECT project_role FROM project_members
             WHERE project_id = ?1 AND user_id = ?2",
            [&project_id, &target_user_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current_s) = current_s else {
        return Err(AppError::NotFound("membership not found".into()));
    };
    let current = ProjectRole::parse(&current_s).unwrap_or(ProjectRole::Viewer);
    if current == ProjectRole::Admin {
        let admins = membership::count_admins(&db, &project_id)
            .map_err(|e| anyhow::anyhow!("count admins: {e}"))?;
        if admins <= 1 {
            return Err(AppError::Conflict(
                "cannot remove the last Project Admin".into(),
            ));
        }
    }

    db.execute(
        "DELETE FROM project_members WHERE project_id = ?1 AND user_id = ?2",
        params![project_id, target_user_id],
    )?;
    drop(db);

    audit::log(
        &state,
        AuthEvent::ProjectMemberRemoved,
        Some(&actor.id),
        None,
        None,
        Some(serde_json::json!({
            "project_id": project_id,
            "user_id": target_user_id,
            "role": current.as_str(),
        })),
    );
    Ok(Json(serde_json::json!({"ok": true})))
}
