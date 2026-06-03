//! CRUD for the declared service dependency graph (Layer C).
//!
//! Operators declare "this service depends_on that service" edges so a push
//! that redeploys the dependency also redeploys its dependents (see
//! [`crate::deploy::deps`]). Edges are constrained to a single project and are
//! cycle-checked at write time (the runtime closure walk is cycle-safe anyway,
//! but rejecting cycles here keeps the graph debuggable).

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/resources/{id}/dependencies — list this service's dependencies
/// (the services it depends_on) with their names for display.
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT d.id, d.depends_on_service_id, s.name
         FROM service_dependencies d
         JOIN services s ON s.id = d.depends_on_service_id
         WHERE d.service_id = ?1
         ORDER BY s.name",
    )?;
    let deps: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "depends_on_service_id": row.get::<_, String>(1)?,
                "depends_on_name": row.get::<_, String>(2)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(deps))
}

#[derive(Deserialize)]
pub struct AddDependencyRequest {
    pub depends_on_service_id: String,
}

/// POST /api/v1/resources/{id}/dependencies — declare that `{id}` depends_on
/// `depends_on_service_id`. Validates same-project, non-self, and acyclic.
pub async fn add(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<AddDependencyRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;

    let target = body.depends_on_service_id.trim().to_string();
    if target.is_empty() {
        return Err(AppError::BadRequest(
            "depends_on_service_id is required".into(),
        ));
    }
    if target == id {
        return Err(AppError::BadRequest(
            "a service cannot depend on itself".into(),
        ));
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Both services must exist and share a project — cross-project dependency
    // edges are a footgun (and would let an Editor on project A wire a redeploy
    // of project B).
    let project_a: Option<String> = db
        .query_row(
            "SELECT project_id FROM services WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .map_err(|_| AppError::NotFound(format!("Service {id} not found")))?;
    let project_b: Option<String> = db
        .query_row(
            "SELECT project_id FROM services WHERE id = ?1",
            [&target],
            |r| r.get(0),
        )
        .map_err(|_| AppError::BadRequest("target service not found".into()))?;
    if project_a != project_b {
        return Err(AppError::BadRequest(
            "dependency must be within the same project".into(),
        ));
    }

    // Cycle guard: if `target` already (transitively) depends on `id`, then
    // adding `id depends_on target` would close a loop. expand_with_dependents
    // from `id` is exactly the set of services that depend on `id`.
    let closure = crate::deploy::deps::expand_with_dependents(&db, std::slice::from_ref(&id))
        .unwrap_or_default();
    if closure.contains(&target) {
        return Err(AppError::Conflict(
            "that edge would create a dependency cycle".into(),
        ));
    }

    let dep_id = uuid::Uuid::new_v4().to_string();
    match db.execute(
        "INSERT INTO service_dependencies (id, service_id, depends_on_service_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![dep_id, id, target],
    ) {
        Ok(_) => Ok(Json(serde_json::json!({"ok": true, "id": dep_id}))),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(AppError::Conflict("dependency already exists".into()))
        }
        Err(e) => Err(AppError::Database(e)),
    }
}

/// DELETE /api/v1/resources/{id}/dependencies/{dep_id} — remove one edge.
pub async fn remove(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dep_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let n = db.execute(
        "DELETE FROM service_dependencies WHERE id = ?1 AND service_id = ?2",
        rusqlite::params![dep_id, id],
    )?;
    if n == 0 {
        return Err(AppError::NotFound(format!("Dependency {dep_id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}
