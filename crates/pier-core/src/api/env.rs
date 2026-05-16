use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/resources/{id}/env — read environment variables. Editor+ only
/// — env contains secrets, plain Viewers should not be able to read it.
pub async fn get_env(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
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

    let decrypted = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env: HashMap<String, String> = serde_json::from_str(&decrypted).unwrap_or_default();

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
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEnvRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let env_json_plain = serde_json::to_string(&body.env)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON serialize: {e}")))?;
    let env_json = crate::crypto::encrypt_env_json(&env_json_plain);

    // Get current resource info
    let (name, compose_content, catalog_id, git_repo_url, git_branch) = {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "UPDATE services SET env_json = ?1, env_dirty = 1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![env_json, id],
        )?;
        db.query_row(
            "SELECT name, compose_content, catalog_id, git_repo_url, git_branch FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    // Redeploy if requested
    if body.redeploy {
        // Git-based services: run full pipeline
        if let Some(repo_url) = &git_repo_url {
            if !repo_url.is_empty() {
                let branch = git_branch.unwrap_or_else(|| "main".to_string());
                let commit = crate::deploy::CommitInfo {
                    sha: "env-redeploy".to_string(),
                    message: "Save & Redeploy (env update)".to_string(),
                    branch,
                };
                {
                    let db = state
                        .db
                        .lock()
                        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                    let _ = db.execute("UPDATE services SET status = 'deploying', updated_at = datetime('now') WHERE id = ?1", [&id]);
                }
                let state_clone = std::sync::Arc::clone(&state);
                let sid = id.clone();
                tokio::spawn(async move {
                    crate::deploy::run_pipeline(state_clone, sid, commit, "redeploy").await;
                });
                return Ok(Json(serde_json::json!({"ok": true, "redeploying": true})));
            }
        }

        // Catalog-based services: use compose YAML
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

            // Resolve network name
            let network_name: Option<String> = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                db.query_row(
                    "SELECT n.name FROM networks n JOIN services s ON s.network_id = n.id WHERE s.id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .ok()
            };

            // Build new compose YAML
            let new_yaml = if let Some(item) = catalog_item {
                if let Some(compose) = &item.compose {
                    crate::catalog::build_from_template(&compose.template, &body.env)
                } else {
                    crate::catalog::build_compose_yaml(
                        item,
                        &id,
                        &name,
                        &body.env,
                        &ports,
                        network_name.as_deref(),
                    )
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
            let result =
                docker::deploy_service_stack(&state, &id, &stack_name, &new_yaml, None).await;
            let status = if result.is_ok() { "running" } else { "failed" };
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                // On success clear env_dirty — the running container now has the new env
                let dirty_reset = if status == "running" { 0 } else { 1 };
                db.execute(
                    "UPDATE services SET status = ?1, env_dirty = ?2, updated_at = datetime('now') WHERE id = ?3",
                    rusqlite::params![status, dirty_reset, id],
                )?;
            }
            result.map_err(|e| AppError::Internal(anyhow::anyhow!("Redeploy failed: {e}")))?;
        }
    }

    Ok(Json(serde_json::json!({"ok": true})))
}
