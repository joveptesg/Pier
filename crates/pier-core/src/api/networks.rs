use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/networks
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT n.id, n.name, n.description, n.driver, n.is_default, n.created_at,
                (SELECT COUNT(*) FROM services WHERE network_id = n.id) as service_count
         FROM networks n ORDER BY n.is_default DESC, n.name",
    )?;

    let networks: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "description": row.get::<_, String>(2)?,
                "driver": row.get::<_, String>(3)?,
                "is_default": row.get::<_, i64>(4)? != 0,
                "created_at": row.get::<_, String>(5)?,
                "service_count": row.get::<_, i64>(6)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(networks))
}

/// POST /api/v1/networks
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateNetworkRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Network name is required".into()));
    }

    // Validate name (Docker network naming rules)
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::BadRequest(
            "Network name may only contain letters, numbers, hyphens, underscores".into(),
        ));
    }

    // Create Docker network
    use bollard::models::NetworkCreateRequest;
    let docker_name = format!("pier-{name}");
    let driver = body.driver.as_deref().unwrap_or("bridge");

    // Check if Docker network already exists
    if state.docker.inspect_network(&docker_name, None).await.is_err() {
        state
            .docker
            .create_network(NetworkCreateRequest {
                name: docker_name.clone(),
                driver: Some(driver.to_string()),
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("Docker network create: {e}"))?;
        tracing::info!("Created Docker network: {docker_name}");
    }

    // Save to DB
    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO networks (id, name, description, driver)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, docker_name, body.description.unwrap_or_default(), driver],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": id, "name": docker_name})))
}

/// DELETE /api/v1/networks/{id}
pub async fn delete(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        // Check if default
        let (name, is_default): (String, bool) = db
            .query_row(
                "SELECT name, is_default FROM networks WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .map_err(|_| AppError::NotFound(format!("Network {id} not found")))?;

        if is_default {
            return Err(AppError::BadRequest(
                "Cannot delete the default network".into(),
            ));
        }

        // Check if any services use this network
        let count: i64 = db.query_row(
            "SELECT COUNT(*) FROM services WHERE network_id = ?1",
            [&id],
            |row| row.get(0),
        )?;
        if count > 0 {
            return Err(AppError::BadRequest(format!(
                "Network has {count} service(s). Move them first."
            )));
        }

        // Remove from DB
        db.execute("DELETE FROM networks WHERE id = ?1", [&id])?;
        name
    };

    // Remove Docker network (ignore errors — may not exist)
    let _ = state.docker.remove_network(&name).await;
    tracing::info!("Deleted Docker network: {name}");

    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct CreateNetworkRequest {
    pub name: String,
    pub description: Option<String>,
    pub driver: Option<String>,
}
