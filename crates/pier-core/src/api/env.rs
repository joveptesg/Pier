use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;

use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/resources/{id}/env — read environment variables.
pub async fn get_env(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let env_json: Option<String> = db
        .query_row(
            "SELECT env_json FROM services WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?;

    let env: HashMap<String, String> = env_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();

    Ok(Json(serde_json::json!(env)))
}

#[derive(Deserialize)]
pub struct UpdateEnvRequest {
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub redeploy: bool,
}

/// PUT /api/v1/resources/{id}/env — update env vars and optionally redeploy.
pub async fn update_env(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEnvRequest>,
) -> AppResult<impl IntoResponse> {
    let env_json = serde_json::to_string(&body.env)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON serialize: {e}")))?;

    // Get current resource info
    let (name, compose_content, catalog_id) = {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "UPDATE services SET env_json = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![env_json, id],
        )?;
        db.query_row(
            "SELECT name, compose_content, catalog_id FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    // Redeploy if requested and compose_content exists
    if body.redeploy {
        if let Some(yaml) = &compose_content {
            // Rebuild compose YAML with new env vars
            let catalog_item = catalog_id
                .as_ref()
                .and_then(|cid| state.catalog.iter().find(|i| i.meta.id == *cid));

            // Get ports
            let ports: Vec<(String, u16, u16)> = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                let mut stmt = db.prepare(
                    "SELECT port_name, host_port, container_port FROM port_allocations WHERE service_id = ?1"
                )?;
                let result: Vec<(String, u16, u16)> = stmt
                    .query_map([&id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)? as u16,
                            row.get::<_, i64>(2)? as u16,
                        ))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                result
            };

            // Build new compose YAML
            let new_yaml = if let Some(item) = catalog_item {
                if let Some(compose) = &item.compose {
                    crate::catalog::build_from_template(&compose.template, &body.env)
                } else {
                    crate::catalog::build_compose_yaml(item, &id, &name, &body.env, &ports)
                }
            } else {
                yaml.clone()
            };

            let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

            // Update compose_content in DB
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                db.execute(
                    "UPDATE services SET compose_content = ?1, status = 'deploying', updated_at = datetime('now') WHERE id = ?2",
                    rusqlite::params![new_yaml, id],
                )?;
            }

            // Redeploy
            let result = docker::compose::deploy_stack(&stack_name, &new_yaml, &state.config).await;
            let status = if result.is_ok() { "running" } else { "failed" };
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                db.execute(
                    "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                    rusqlite::params![status, id],
                )?;
            }
            result.map_err(|e| AppError::Internal(anyhow::anyhow!("Redeploy failed: {e}")))?;
        }
    }

    Ok(Json(serde_json::json!({"ok": true})))
}
