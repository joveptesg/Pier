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

/// Validate a project port range and reject overlaps with existing projects.
///
/// - `None`/`None` means "no range" and is always accepted (services fall back
///   to the global config range).
/// - One side set without the other is rejected: a half-open range is almost
///   always a UI typo.
/// - Range bounds are inclusive; two projects with `[5000,5099]` and
///   `[5100,5199]` are considered non-overlapping.
/// - `exclude_project_id` lets the update handler skip the row being edited.
fn validate_port_range(
    db: &rusqlite::Connection,
    start: Option<i64>,
    end: Option<i64>,
    exclude_project_id: Option<&str>,
) -> Result<(), AppError> {
    let (start, end) = match (start, end) {
        (None, None) => return Ok(()),
        (Some(s), Some(e)) => (s, e),
        _ => {
            return Err(AppError::BadRequest(
                "port_range_start and port_range_end must be set together".into(),
            ));
        }
    };

    if !(1024..=65535).contains(&start) || !(1024..=65535).contains(&end) {
        return Err(AppError::BadRequest(format!(
            "Port range must be within 1024-65535 (got {start}-{end})"
        )));
    }
    if start > end {
        return Err(AppError::BadRequest(format!(
            "Port range start must be <= end (got {start}-{end})"
        )));
    }

    let mut stmt = db.prepare(
        "SELECT id, name, port_range_start, port_range_end
         FROM projects
         WHERE port_range_start IS NOT NULL AND port_range_end IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    for row in rows {
        let (id, name, other_start, other_end) = row?;
        if let Some(skip_id) = exclude_project_id {
            if id == skip_id {
                continue;
            }
        }
        // Inclusive overlap: not (end < other_start OR start > other_end)
        if !(end < other_start || start > other_end) {
            return Err(AppError::Conflict(format!(
                "Port range {start}-{end} overlaps with project '{name}' ({other_start}-{other_end})"
            )));
        }
    }

    Ok(())
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

    validate_port_range(&db, body.port_range_start, body.port_range_end, None)?;

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

    // Resolve effective new port range = body value if set, otherwise the
    // project's existing value. We need both sides to do the overlap and
    // narrow-conflict checks.
    let (current_start, current_end): (Option<i64>, Option<i64>) = db
        .query_row(
            "SELECT port_range_start, port_range_end FROM projects WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Project {id} not found")))?;
    let new_start = body.port_range_start.or(current_start);
    let new_end = body.port_range_end.or(current_end);

    if body.port_range_start.is_some() || body.port_range_end.is_some() {
        validate_port_range(&db, new_start, new_end, Some(&id))?;

        // If the new range actually changed and is fully specified, refuse to
        // shrink it past existing allocations — narrowing a range that's
        // already in use would silently desync the project's invariant.
        if let (Some(ns), Some(ne)) = (new_start, new_end) {
            if (new_start, new_end) != (current_start, current_end) {
                let mut stmt = db.prepare(
                    "SELECT s.name, pa.host_port
                     FROM port_allocations pa
                     JOIN services s ON s.id = pa.service_id
                     WHERE s.project_id = ?1 AND (pa.host_port < ?2 OR pa.host_port > ?3)
                     ORDER BY pa.host_port",
                )?;
                let offending: Vec<(String, i64)> = stmt
                    .query_map(rusqlite::params![&id, ns, ne], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                if !offending.is_empty() {
                    let summary = offending
                        .iter()
                        .map(|(n, p)| format!("{n}:{p}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(AppError::Conflict(format!(
                        "Cannot narrow port range to {ns}-{ne}: services using ports outside new range [{summary}]"
                    )));
                }
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                port_range_start INTEGER,
                port_range_end INTEGER,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .expect("create projects table");
        conn
    }

    fn insert_project(
        conn: &Connection,
        id: &str,
        name: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO projects (id, name, port_range_start, port_range_end) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, name, start, end],
        )
        .expect("insert project");
    }

    #[test]
    fn allows_both_none() {
        let conn = test_db();
        assert!(validate_port_range(&conn, None, None, None).is_ok());
    }

    #[test]
    fn rejects_only_one_side_set() {
        let conn = test_db();
        let err = validate_port_range(&conn, Some(5000), None, None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        let err = validate_port_range(&conn, None, Some(5100), None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn rejects_inverted_range() {
        let conn = test_db();
        let err = validate_port_range(&conn, Some(6000), Some(5000), None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn rejects_out_of_bounds() {
        let conn = test_db();
        let err = validate_port_range(&conn, Some(80), Some(443), None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        let err = validate_port_range(&conn, Some(60000), Some(70000), None).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn rejects_overlap() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", Some(5000), Some(5100));
        let err = validate_port_range(&conn, Some(5050), Some(5150), None).unwrap_err();
        match err {
            AppError::Conflict(msg) => assert!(msg.contains("alpha")),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[test]
    fn allows_adjacent_ranges() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", Some(5000), Some(5099));
        // [5100, 5199] starts exactly where [5000, 5099] ends + 1 → no overlap.
        assert!(validate_port_range(&conn, Some(5100), Some(5199), None).is_ok());
    }

    #[test]
    fn allows_exact_boundary_outside() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", Some(5000), Some(5100));
        // 5101 starts after 5100; 4999 ends before 5000 — both OK.
        assert!(validate_port_range(&conn, Some(5101), Some(5200), None).is_ok());
        assert!(validate_port_range(&conn, Some(4900), Some(4999), None).is_ok());
    }

    #[test]
    fn detects_containment_either_way() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", Some(5000), Some(5100));
        // New range contained in existing
        assert!(matches!(
            validate_port_range(&conn, Some(5040), Some(5060), None).unwrap_err(),
            AppError::Conflict(_)
        ));
        // New range contains existing
        assert!(matches!(
            validate_port_range(&conn, Some(4900), Some(5200), None).unwrap_err(),
            AppError::Conflict(_)
        ));
    }

    #[test]
    fn excludes_self_on_update() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", Some(5000), Some(5100));
        // Editing "alpha" to a range that overlaps only with itself must succeed.
        assert!(validate_port_range(&conn, Some(5000), Some(5100), Some("a")).is_ok());
        // But still rejects overlap with other projects.
        insert_project(&conn, "b", "beta", Some(6000), Some(6100));
        assert!(matches!(
            validate_port_range(&conn, Some(6050), Some(6200), Some("a")).unwrap_err(),
            AppError::Conflict(_)
        ));
    }

    #[test]
    fn ignores_projects_without_range() {
        let conn = test_db();
        insert_project(&conn, "a", "alpha", None, None);
        // No existing range → no overlap possible.
        assert!(validate_port_range(&conn, Some(5000), Some(5100), None).is_ok());
    }
}
