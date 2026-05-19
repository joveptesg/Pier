use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_project_role, GlobalRole, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

use super::security::{self, DeleteRequest};

#[derive(Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub port_range_start: Option<i64>,
    pub port_range_end: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub port_range_start: Option<i64>,
    pub port_range_end: Option<i64>,
}

/// GET /api/v1/projects
///
/// Global Admin+ and peer requests see every project. Plain Users see only
/// those they're a `project_members` row for.
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let see_all = user.is_peer || user.global_role.at_least(GlobalRole::Admin);

    let projects: Vec<serde_json::Value> = if see_all {
        let mut stmt = db.prepare(
            "SELECT id, name, description, port_range_start, port_range_end, created_at
             FROM projects ORDER BY name",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], project_row)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        let mut stmt = db.prepare(
            "SELECT p.id, p.name, p.description, p.port_range_start, p.port_range_end, p.created_at
             FROM projects p
             JOIN project_members pm ON pm.project_id = p.id
             WHERE pm.user_id = ?1
             ORDER BY p.name",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([&user.id], project_row)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    Ok(Json(projects))
}

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "name": row.get::<_, String>(1)?,
        "description": row.get::<_, String>(2)?,
        "port_range_start": row.get::<_, Option<i64>>(3)?,
        "port_range_end": row.get::<_, Option<i64>>(4)?,
        "created_at": row.get::<_, String>(5)?,
    }))
}

/// POST /api/v1/projects
///
/// Only global Admin+ can create projects; once the project exists, its
/// Project Admins manage membership and inner settings via the project-scoped
/// routes. Peers retain create access (federation mode treats peer-cores as
/// Admin-equivalent for resource ops, see `policy::can`).
pub async fn create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<CreateProjectRequest>,
) -> AppResult<impl IntoResponse> {
    if !user.is_peer && !user.global_role.at_least(GlobalRole::Admin) {
        return Err(AppError::Forbidden("only Admin can create projects".into()));
    }
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("Project name is required".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO projects (id, name, description, port_range_start, port_range_end)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.description,
            body.port_range_start,
            body.port_range_end
        ],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// GET /api/v1/projects/:id
pub async fn get(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    enforce_project_role(&user, &id, ProjectRole::Viewer, &db)?;

    let project = db.query_row(
        "SELECT id, name, description, port_range_start, port_range_end, created_at FROM projects WHERE id = ?1",
        [&id],
        |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "description": row.get::<_, String>(2)?,
                "port_range_start": row.get::<_, Option<i64>>(3)?,
                "port_range_end": row.get::<_, Option<i64>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        },
    ).map_err(|_| AppError::NotFound(format!("Project {id} not found")))?;

    // Get services for this project
    let mut stmt = db.prepare(
        "SELECT s.id, s.name, s.service_type, s.status, s.port, s.image, \
                s.catalog_id, s.category, \
                (SELECT domain FROM domains WHERE service_id = s.id ORDER BY created_at LIMIT 1) AS primary_domain \
         FROM services s WHERE s.project_id = ?1",
    )?;

    let services: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            let catalog_id: Option<String> = row.get(6)?;
            let icon: Option<String> = catalog_id.as_deref().and_then(|cid| {
                state
                    .catalog
                    .iter()
                    .find(|i| i.meta.id == cid)
                    .and_then(|i| i.meta.icon.clone())
            });
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "service_type": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "port": row.get::<_, Option<i64>>(4)?,
                "image": row.get::<_, Option<String>>(5)?,
                "catalog_id": catalog_id,
                "category": row.get::<_, Option<String>>(7)?,
                "icon": icon,
                "primary_domain": row.get::<_, Option<String>>(8)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(serde_json::json!({
        "project": project,
        "services": services,
    })))
}

/// PUT /api/v1/projects/:id
pub async fn update(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    enforce_project_role(&user, &id, ProjectRole::Admin, &db)?;

    // Build dynamic update
    let mut sets = vec!["updated_at = datetime('now')".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(ref name) = body.name {
        sets.push(format!("name = ?{}", params.len() + 1));
        params.push(Box::new(name.clone()));
    }
    if let Some(ref desc) = body.description {
        sets.push(format!("description = ?{}", params.len() + 1));
        params.push(Box::new(desc.clone()));
    }
    if let Some(start) = body.port_range_start {
        sets.push(format!("port_range_start = ?{}", params.len() + 1));
        params.push(Box::new(start));
    }
    if let Some(end) = body.port_range_end {
        sets.push(format!("port_range_end = ?{}", params.len() + 1));
        params.push(Box::new(end));
    }

    params.push(Box::new(id.clone()));
    let sql = format!(
        "UPDATE projects SET {} WHERE id = ?{}",
        sets.join(", "),
        params.len()
    );

    let params_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = db.execute(&sql, params_refs.as_slice())?;

    if rows == 0 {
        return Err(AppError::NotFound(format!("Project {id} not found")));
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/projects/:id
///
/// Refuses to delete projects that still own services — without this guard
/// the FK `services.project_id ON DELETE SET NULL` would orphan running
/// containers and keep their ports allocated. The UI surfaces the count so
/// the user knows what to clean up first.
pub async fn delete(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<DeleteRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    enforce_project_role(&user, &id, ProjectRole::Admin, &db)?;

    let exists: i64 = db.query_row(
        "SELECT COUNT(*) FROM projects WHERE id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Err(AppError::NotFound(format!("Project {id} not found")));
    }

    let svc_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM services WHERE project_id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    if svc_count > 0 {
        return Err(AppError::Conflict(format!(
            "Project has {svc_count} service(s). Delete them first."
        )));
    }

    security::verify_delete_password(&db, &user.id, body.password.as_deref())?;

    let rows = db.execute("DELETE FROM projects WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Project {id} not found")));
    }

    Ok(Json(serde_json::json!({"ok": true, "action": "deleted"})))
}
