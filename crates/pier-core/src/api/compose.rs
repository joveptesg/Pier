use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateStackRequest {
    pub name: String,
    pub yaml: String,
}

#[derive(Deserialize)]
pub struct UpdateStackRequest {
    pub yaml: String,
    /// When true, apply the new YAML to the running stack (writes compose + .env
    /// to disk and runs `docker compose up`). Defaults to false — same opt-in
    /// semantics as `PUT /api/v1/resources/{id}/env`.
    #[serde(default)]
    pub redeploy: bool,
}

/// GET /api/v1/stacks
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, name, compose_content, status, created_at FROM services WHERE service_type = 'compose'"
    )?;

    let stacks: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "has_yaml": row.get::<_, Option<String>>(2)?.is_some(),
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(stacks))
}

/// POST /api/v1/stacks
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateStackRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.yaml.trim().is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.compose.name_and_yaml_required",
        )));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO services (id, name, service_type, compose_content, status)
         VALUES (?1, ?2, 'compose', ?3, 'created')",
        rusqlite::params![id, body.name.trim(), body.yaml],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// GET /api/v1/stacks/:id
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let result = db.query_row(
        "SELECT id, name, compose_content, status FROM services WHERE id = ?1 AND service_type = 'compose'",
        [&id],
        |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "yaml": row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "status": row.get::<_, String>(3)?,
            }))
        },
    ).map_err(|_| AppError::NotFound(crate::i18n::te_args("errors.compose.stack_not_found", &[("id", &id)])))?;

    Ok(Json(result))
}

/// PUT /api/v1/stacks/:id — update a compose stack's YAML, optionally redeploying.
///
/// Mirrors `create_compose`: Pier identity labels are injected into the stored
/// YAML so container discovery (Logs tab, port-sync, recreate, proxy) keeps
/// working — the previous handler wrote the raw YAML and stripped them. With
/// `redeploy: true` the change is applied to the running stack (writes compose +
/// `.env` to disk, runs `docker compose up`, re-syncs ports); the default
/// `false` only persists to the DB, matching `PUT /env` semantics.
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateStackRequest>,
) -> AppResult<impl IntoResponse> {
    // Fetch the target compose service first, so we have its name (for the stack
    // dir) and catalog_id (for label injection), and a clean 404 if it doesn't
    // exist or isn't a compose service.
    let (name, catalog_id): (String, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.compose.stack_not_found",
                &[("id", &id)],
            ))
        })?
    };

    // Inject Pier identity labels (idempotent) so discovery can correlate this
    // stack's containers by pier.service.id. Without this a PUT strips the labels
    // create_compose added, breaking the Logs tab / port-sync / recreate.
    let yaml = crate::deploy::inject_pier_labels(
        &body.yaml,
        &id,
        catalog_id.as_deref().unwrap_or("docker-compose"),
    );

    // Persist the labeled YAML.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE services SET compose_content = ?1, updated_at = datetime('now')
             WHERE id = ?2 AND service_type = 'compose'",
            rusqlite::params![yaml, id],
        )?;
    }

    if !body.redeploy {
        // DB + labels persisted; running container left untouched (same opt-out
        // semantics as PUT /env with redeploy:false).
        return Ok(Json(serde_json::json!({"ok": true})));
    }

    // Apply to the running stack. Use the `pier-{slug}` stack-name convention
    // that create_compose / write_env_file / persist_container_name use — a
    // docker-compose resource lives in stacks/pier-{slug}/, not stacks/{name}/.
    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    // deploy_service_stack materializes .env + docker-compose.yml on disk and
    // runs `docker compose up`, so DB and disk end up consistent.
    let result = docker::deploy_service_stack(&state, &id, &stack_name, &yaml, auth).await;
    if result.is_ok() {
        // Record the real docker-compose container name and re-sync ports so the
        // Logs tab and API reflect the updated stack.
        crate::deploy::persist_container_name(&state, &id, &stack_name).await;
        crate::deploy::update_ports_from_compose(&state, &id, &yaml);
    }
    let status = if result.is_ok() { "running" } else { "failed" };
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![status, id],
        );
    }
    result.map_err(|e| AppError::Internal(anyhow::anyhow!("Redeploy failed: {e}")))?;

    Ok(Json(serde_json::json!({"ok": true, "redeployed": true})))
}

/// POST /api/v1/stacks/:id/deploy
pub async fn deploy(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, compose_content FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.compose.stack_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let yaml =
        yaml.ok_or_else(|| AppError::BadRequest(crate::i18n::te("errors.compose.stack_no_yaml")))?;

    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    let output = docker::deploy_service_stack(&state, &id, &name, &yaml, auth).await?;

    // Populate port_allocations / services.port from the compose `ports:` so
    // this standalone stack reports its ports like every other deploy path.
    crate::deploy::update_ports_from_compose(&state, &id, &yaml);

    // Update status
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'running', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    Ok(Json(serde_json::json!({"ok": true, "output": output})))
}

/// POST /api/v1/stacks/:id/down
pub async fn down(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.compose.stack_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let output = docker::compose::down_stack(&name, &state.config).await?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'stopped', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    Ok(Json(serde_json::json!({"ok": true, "output": output})))
}

/// DELETE /api/v1/stacks/:id
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.compose.stack_not_found",
                &[("id", &id)],
            ))
        })?
    };

    // Down first, ignore errors
    let _ = docker::compose::down_stack(&name, &state.config).await;
    let _ = docker::compose::remove_stack(&name, &state.config).await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute("DELETE FROM services WHERE id = ?1", [&id])?;

    Ok(Json(serde_json::json!({"ok": true, "action": "removed"})))
}
