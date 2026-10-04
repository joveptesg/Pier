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
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.env.resource_not_found",
                &[("v", &id)],
            ))
        })?;

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
    apply_env_update(&state, &id, body.env, body.redeploy).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// True for keys the env editor hides: anything with a lowercase letter. Those
/// are catalog template vars (`version`, `name`, `password`, `port`, …) that
/// the compose YAML is regenerated from, not container environment.
fn is_template_var(key: &str) -> bool {
    key.chars().any(|c| c.is_ascii_lowercase())
}

/// Carry the stored template vars over into an incoming env map.
///
/// The editor only round-trips UPPERCASE keys, so a plain save used to drop
/// `version` & co. from `env_json`; the next regeneration then emitted
/// `image: mongo:{{version}}` and the redeploy failed. Keys the caller sends
/// explicitly still win.
fn keep_template_vars(
    state: &SharedState,
    id: &str,
    mut env: HashMap<String, String>,
) -> AppResult<HashMap<String, String>> {
    let (stored, image, catalog_id): (Option<String>, Option<String>, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.query_row(
            "SELECT env_json, image, catalog_id FROM services WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((None, None, None))
    };
    let stored: HashMap<String, String> =
        serde_json::from_str(&crate::crypto::decrypt_env_json(stored.as_deref()))
            .unwrap_or_default();
    for (k, v) in stored {
        if is_template_var(&k) {
            env.entry(k).or_insert(v);
        }
    }
    // Services saved before this fix already lost `version`; recover it from
    // the image they were created with (template `mongo:{{version}}` +
    // stored `mongo:9.0` → `9.0`).
    if !env.contains_key("version") {
        let template = catalog_id
            .as_deref()
            .and_then(|cid| state.catalog.iter().find(|i| i.meta.id == cid))
            .and_then(|item| item.docker.as_ref())
            .map(|d| d.image.as_str());
        if let Some(v) = template
            .zip(image.as_deref())
            .and_then(|(t, i)| version_from_image(t, i))
        {
            env.insert("version".to_string(), v);
        }
    }
    Ok(env)
}

/// The `{{version}}` part of `image`, matched against the catalog's `template`.
fn version_from_image(template: &str, image: &str) -> Option<String> {
    let (prefix, suffix) = template.split_once("{{version}}")?;
    let v = image.strip_prefix(prefix)?.strip_suffix(suffix)?;
    (!v.is_empty()).then(|| v.to_string())
}

/// Persist a service's env vars (encrypted), keep the on-disk `.env` in step,
/// and optionally redeploy — the whole body of the `PUT .../env` handler minus
/// the authorization check.
///
/// Split out so platform-side repairs can reuse the exact same path instead of
/// re-implementing compose regeneration. [`crate::api::pgdata_repair`] calls it
/// to pin `PGDATA` after relocating a cluster; going through here means the
/// repaired service is redeployed by the same code an operator's "Save &
/// Redeploy" click would run, with no second implementation to drift.
///
/// Callers are responsible for authorization.
pub(crate) async fn apply_env_update(
    state: &SharedState,
    id: &str,
    env: HashMap<String, String>,
    redeploy: bool,
) -> AppResult<()> {
    let state = state.clone();
    let id = id.to_string();
    let env = keep_template_vars(&state, &id, env)?;
    let body = UpdateEnvRequest { env, redeploy };

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
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.env.resource_not_found",
                &[("v", &id)],
            ))
        })?
    };

    // Keep the on-disk .env in lockstep with env_json even when the caller opts
    // out of a redeploy. Otherwise the DB and disk silently diverge: a later
    // container recreate (docker restart, daemon crash, image pull, Pier
    // restart) makes `docker compose up` read the stale .env, so the container
    // starts with old env vars. `write_env_file` writes atomically to the same
    // `stacks/{stack_name}/.env` that the deploy path materializes (issue #8).
    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    crate::deploy::write_env_file(&state, &id, &stack_name).await;

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
                return Ok(());
            }
        }

        // Catalog-based services: use compose YAML
        if let Some(yaml) = &compose_content {
            // Rebuild compose YAML with new env vars
            let catalog_item = catalog_id
                .as_ref()
                .and_then(|cid| state.catalog.iter().find(|i| i.meta.id == *cid));

            // Get ports — include the public_port flag so the rebuilt compose
            // YAML keeps the operator-toggled `0.0.0.0:{public}:{container}`
            // mapping. Without this, an env-redeploy would silently drop the
            // public Docker port binding.
            let ports: Vec<crate::catalog::ReplicaPortMapping> = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
                let mut stmt = db.prepare(
                    "SELECT port_name, host_port, container_port, is_public, public_port \
                     FROM port_allocations WHERE service_id = ?1",
                )?;
                let result: Vec<crate::catalog::ReplicaPortMapping> = stmt
                    .query_map([&id], |row| {
                        let is_public: i64 = row.get(3)?;
                        let public_port: Option<i64> = row.get(4)?;
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)? as u16,
                            row.get::<_, i64>(2)? as u16,
                            if is_public == 1 {
                                public_port.map(|p| p as u16)
                            } else {
                                None
                            },
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

            // Build new compose YAML. Passthrough catalog types (e.g.
            // "docker-compose") have no generator — the stored YAML *is* the
            // source of truth. Regenerating them would call
            // `build_compose_yaml`, which returns "" when the catalog item has
            // no `[docker]` section, wiping the user's compose. So keep the
            // stored YAML untouched for those.
            let is_passthrough = catalog_item
                .map(|item| item.compose.is_none() && item.docker.is_none())
                .unwrap_or(true);
            let new_yaml = match catalog_item {
                Some(item) if item.compose.is_some() => crate::catalog::build_from_template(
                    &item.compose.as_ref().unwrap().template,
                    &body.env,
                ),
                Some(item) if item.docker.is_some() => crate::catalog::build_compose_yaml(
                    item,
                    &id,
                    &name,
                    &body.env,
                    &ports,
                    network_name.as_deref(),
                ),
                // Passthrough catalog / non-catalog: preserve the user's YAML.
                _ => yaml.clone(),
            };

            // Passthrough compose stacks are deployed as-is and would otherwise
            // carry no Pier identity labels, so container discovery (logs,
            // port-sync, recreate) can't correlate them. Inject the labels into
            // the persisted YAML. Idempotent: a no-op if they already exist.
            let new_yaml = if is_passthrough {
                crate::deploy::inject_pier_labels(
                    &new_yaml,
                    &id,
                    catalog_id.as_deref().unwrap_or("docker-compose"),
                )
            } else {
                new_yaml
            };

            // Belt-and-suspenders: never persist an empty compose. This can
            // only happen via a future generator regression, but the cost of
            // guarding is trivial next to the cost of destroying user YAML.
            if new_yaml.trim().is_empty() {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "refusing to persist empty compose YAML for service {id}"
                )));
            }

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
            if result.is_ok() {
                // Record the real docker-compose container name so the Logs
                // tab and container discovery resolve it after an env update.
                crate::deploy::persist_container_name(&state, &id, &stack_name).await;
                // Re-sync port_allocations / services.port in case the env
                // change altered `${VAR}`-templated ports in the compose file.
                crate::deploy::update_ports_from_compose(&state, &id, &new_yaml);
            }
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_vars_are_the_keys_the_editor_hides() {
        for k in ["version", "name", "password", "db_password", "ssh_port"] {
            assert!(is_template_var(k), "{k}");
        }
        for k in [
            "POSTGRES_DB",
            "GLIBC_TUNABLES",
            "MONGO_INITDB_ROOT_USERNAME",
        ] {
            assert!(!is_template_var(k), "{k}");
        }
    }

    #[test]
    fn version_is_recovered_from_the_image() {
        assert_eq!(
            version_from_image("mongo:{{version}}", "mongo:9.0").as_deref(),
            Some("9.0")
        );
        assert_eq!(
            version_from_image("qdrant/qdrant:v{{version}}", "qdrant/qdrant:v1.19.1").as_deref(),
            Some("1.19.1")
        );
        assert_eq!(version_from_image("mongo:{{version}}", "redis:8.4"), None);
        assert_eq!(version_from_image("nginx:latest", "nginx:latest"), None);
    }
}
