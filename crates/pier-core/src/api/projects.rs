use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

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
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, name, description, port_range_start, port_range_end, created_at FROM projects ORDER BY name"
    )?;

    let projects: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "description": row.get::<_, String>(2)?,
                "port_range_start": row.get::<_, Option<i64>>(3)?,
                "port_range_end": row.get::<_, Option<i64>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(projects))
}

/// POST /api/v1/projects
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateProjectRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("Project name is required".into()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let db = state.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO projects (id, name, description, port_range_start, port_range_end)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, body.name.trim(), body.description, body.port_range_start, body.port_range_end],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// GET /api/v1/projects/:id
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

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
        "SELECT id, name, service_type, status, port, image FROM services WHERE project_id = ?1"
    )?;

    let services: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "service_type": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "port": row.get::<_, Option<i64>>(4)?,
                "image": row.get::<_, Option<String>>(5)?,
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
    Path(id): Path<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

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
pub async fn delete(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state.db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute("DELETE FROM projects WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Project {id} not found")));
    }

    Ok(Json(serde_json::json!({"ok": true, "action": "deleted"})))
}
