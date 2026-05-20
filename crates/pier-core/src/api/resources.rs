use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_project_role, enforce_resource_role, GlobalRole, ProjectRole};
use crate::catalog;
use crate::catalog::cluster as cluster_gen;
use crate::db::ports;
use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

use super::domains;
use super::security::{self, DeleteRequest};

/// Pick the best port for HTTP proxy from a list of allocated ports.
/// Prefers ports named "management", "http", "web", "ui"; falls back to first port.
fn pick_http_port(ports: &[crate::db::models::PortAllocation]) -> Option<i64> {
    let http_keywords = ["management", "http", "web", "ui", "dashboard", "console"];
    ports
        .iter()
        .find(|p| {
            http_keywords
                .iter()
                .any(|k| p.port_name.to_lowercase().contains(k))
        })
        .or(ports.first())
        .map(|p| p.container_port)
}

/// Try to auto-generate a service domain (sslip.io) after deploy.
/// Non-blocking: logs errors but does not fail the deploy.
async fn try_create_service_domain(state: &SharedState, service_id: &str, name: &str, port: i64) {
    match domains::create_service_domain(state, service_id, name, port as i32).await {
        Ok(domain) if !domain.is_empty() => {
            tracing::info!("Auto-generated domain for {name}: {domain}");
        }
        Ok(_) => {} // proxy disabled, no domain
        Err(e) => {
            tracing::warn!("Failed to auto-generate domain for {name}: {e}");
        }
    }
}

#[derive(Deserialize)]
pub struct CreateResourceRequest {
    pub catalog_id: String,
    pub name: String,
    pub project_id: Option<String>,
    pub network_id: Option<String>,
    /// Target server ID (default: "local")
    pub server_id: Option<String>,
    #[serde(default)]
    pub config: HashMap<String, String>,
    /// Git source ID for GitHub App deploys
    pub source_id: Option<String>,
    /// "standalone" (default) or "cluster"
    pub deployment_mode: Option<String>,
    /// Number of nodes for cluster mode
    pub node_count: Option<usize>,
    /// Node-to-server distribution: [{"server_id": "srv-xxx"}, ...]
    #[serde(default)]
    pub node_distribution: Vec<NodeAssignment>,
}

#[derive(Deserialize, Clone)]
pub struct NodeAssignment {
    pub server_id: String,
}

#[derive(Deserialize)]
pub struct ScaleRequest {
    /// New total node count
    pub node_count: usize,
    /// Server ID for the new node (when scaling up)
    pub server_id: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct LoadBalanceSlot {
    pub server_id: String,
    pub replicas: i64,
    #[serde(default = "default_weight")]
    pub weight: i64,
}

fn default_weight() -> i64 {
    1
}

#[derive(Deserialize)]
pub struct LoadBalanceRequest {
    /// Strategy: "round-robin" (default), "weighted", or "sticky".
    #[serde(default)]
    pub strategy: Option<String>,
    /// Cookie name when strategy == "sticky".
    #[serde(default)]
    pub sticky_cookie: Option<String>,
    /// Explicit per-server placement. Takes precedence over `replicas`.
    #[serde(default)]
    pub distribution: Vec<LoadBalanceSlot>,
    /// Shortcut: total replicas on the service's current server. Used when
    /// `distribution` is empty.
    #[serde(default)]
    pub replicas: Option<i64>,
}

/// Helper: lock DB, run a closure, drop the guard.
fn with_db<F, R>(state: &SharedState, f: F) -> AppResult<R>
where
    F: FnOnce(&rusqlite::Connection) -> AppResult<R>,
{
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    f(&db)
}

/// Reject create when a service with the same (project_id, name) already exists.
/// Returns `AppError::ResourceNameConflict` carrying the existing service id so the UI
/// can offer "Open existing" / Retry instead of silently creating a duplicate row.
fn check_name_available(
    state: &SharedState,
    project_id: Option<&str>,
    name: &str,
) -> AppResult<()> {
    let pid = project_id.unwrap_or("").to_string();
    let name = name.to_string();
    let existing: Option<String> = with_db(state, |db| {
        Ok(db
            .query_row(
                "SELECT id FROM services WHERE COALESCE(project_id,'') = ?1 AND name = ?2 LIMIT 1",
                rusqlite::params![pid, name],
                |row| row.get::<_, String>(0),
            )
            .ok())
    })?;
    if let Some(existing_id) = existing {
        return Err(AppError::ResourceNameConflict { name, existing_id });
    }
    Ok(())
}

/// POST /api/v1/resources — create and deploy a resource from catalog.
pub async fn create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<CreateResourceRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }
    // Project membership gate: if a project is supplied, the caller needs at
    // least Editor on it. Standalone resources (no project_id) remain admin-only.
    match body.project_id.as_deref() {
        Some(pid) => {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            enforce_project_role(&user, pid, ProjectRole::Editor, &db)?;
        }
        None => {
            if !user.is_peer && !user.global_role.at_least(GlobalRole::Admin) {
                return Err(AppError::Forbidden(
                    "creating resources without a project requires Admin".into(),
                ));
            }
        }
    }

    // Block duplicates up-front: if a service with the same (project_id, name) already exists
    // (incl. failed/deploying), return 409 with the existing id so the UI can route the user
    // to Retry/Delete instead of silently creating a sibling row.
    check_name_available(&state, body.project_id.as_deref(), &name)?;

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    // Find catalog template
    let item = state
        .catalog
        .iter()
        .find(|i| i.meta.id == body.catalog_id)
        .ok_or_else(|| {
            AppError::NotFound(format!("Catalog template '{}' not found", body.catalog_id))
        })?
        .clone();

    // ── Docker-compose (raw YAML) ────────────────────────────
    if body.catalog_id == "docker-compose" {
        return create_compose(&state, &body, &name, &stack_name, &item).await;
    }

    // ── Dockerfile build + deploy ────────────────────────────
    if body.catalog_id == "dockerfile" {
        return create_dockerfile(&state, &body, &name, &stack_name, &item).await;
    }

    // ── Git-based deploy (public repo) ────────────────────────
    if body.catalog_id == "git-public" {
        return create_git_deploy(&state, &body, &name, &stack_name, &item, false).await;
    }

    // ── Git-based deploy (private repo with deploy key) ───────
    if body.catalog_id == "git-private-key" {
        return create_git_deploy(&state, &body, &name, &stack_name, &item, true).await;
    }

    // ── Git-based deploy (GitHub App) ──────────────────────────
    if body.catalog_id == "git-github-app" {
        return create_git_deploy_github_app(&state, &body, &name, &stack_name, &item).await;
    }

    // ── Standard catalog deploy ──────────────────────────────
    let service_id = uuid::Uuid::new_v4().to_string();

    // Build variables map
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("name".to_string(), name.clone());

    if let Some(versions) = &item.versions {
        let version = body
            .config
            .get("version")
            .cloned()
            .unwrap_or_else(|| versions.default.clone());
        vars.insert("version".to_string(), version);
    }

    // Generate passwords for auto_generate fields
    if let Some(ui) = &item.ui {
        for field in ui.fields.values() {
            if field.auto_generate {
                let password = body
                    .config
                    .get(&field.maps_to.clone().unwrap_or_default())
                    .cloned()
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| catalog::generate_password(24));
                vars.insert("password".to_string(), password.clone());
                if let Some(maps_to) = &field.maps_to {
                    vars.insert(maps_to.clone(), password);
                }
            }
        }
    }

    // Map user config to env vars
    if let Some(ui) = &item.ui {
        for (key, field) in &ui.fields {
            if let Some(maps_to) = &field.maps_to {
                if let Some(val) = body.config.get(key) {
                    if !val.is_empty() {
                        vars.insert(maps_to.clone(), val.clone());
                    }
                }
            }
        }
    }

    // Apply env defaults
    let env_defaults: Vec<(String, String)> = item
        .env
        .iter()
        .filter(|(k, _)| !vars.contains_key(*k))
        .map(|(k, v)| {
            let val = v
                .default
                .as_ref()
                .map(|d| catalog::substitute(d, &vars))
                .unwrap_or_default();
            (k.clone(), val)
        })
        .collect();
    for (k, v) in env_defaults {
        vars.insert(k, v);
    }

    // Determine port pool
    let (port_start, port_end) = with_db(&state, |db| {
        if let Some(pid) = &body.project_id {
            let range = db.query_row(
                "SELECT port_range_start, port_range_end FROM projects WHERE id = ?1",
                [pid],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            );
            match range {
                Ok((Some(s), Some(e))) => Ok((s as u16, e as u16)),
                _ => Ok((state.config.port_range_start, state.config.port_range_end)),
            }
        } else {
            Ok((state.config.port_range_start, state.config.port_range_end))
        }
    })?;

    // Allocate ports
    let port_specs: Vec<(String, u16)> = if body.catalog_id == "docker-image" {
        let container_port: u16 = body
            .config
            .get("port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(80);
        vec![("primary".to_string(), container_port)]
    } else {
        item.ports
            .iter()
            .map(|(pname, p)| (pname.clone(), p.internal))
            .collect()
    };

    // Resolve network_id: use provided or fall back to default
    let network_id = body.network_id.clone().or_else(|| {
        state.db.lock().ok().and_then(|db| {
            db.query_row(
                "SELECT id FROM networks WHERE is_default = 1 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok()
        })
    });

    // Resolve network name for compose YAML
    let network_name: Option<String> = network_id.as_ref().and_then(|nid| {
        state.db.lock().ok().and_then(|db| {
            db.query_row("SELECT name FROM networks WHERE id = ?1", [nid], |row| {
                row.get(0)
            })
            .ok()
        })
    });

    let allocated_ports = with_db(&state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, network_id, server_id, name, service_type, status, catalog_id, category, env_json, image)
             VALUES (?1, ?2, ?3, ?4, ?5, 'compose', 'deploying', ?6, ?7, ?8, ?9)",
            rusqlite::params![
                service_id,
                body.project_id,
                network_id,
                body.server_id.as_deref().unwrap_or("local"),
                name,
                body.catalog_id,
                item.meta.category,
                crate::crypto::encrypt_env_json(&serde_json::to_string(&vars).unwrap_or_else(|_| "{}".into())),
                item.docker.as_ref().map(|d| catalog::substitute(&d.image, &vars)),
            ],
        )?;
        Ok(ports::allocate_ports(
            db,
            &service_id,
            &port_specs,
            port_start,
            port_end,
        )?)
    })?;

    // Add allocated ports to vars. New services start with is_public=0, so
    // public_port slot stays None — `set_port_public` rebuilds the compose
    // YAML with the public mapping later if the operator toggles it on.
    let port_mappings: Vec<catalog::ReplicaPortMapping> = allocated_ports
        .iter()
        .map(|pa| {
            vars.insert(format!("port_{}", pa.port_name), pa.host_port.to_string());
            (
                pa.port_name.clone(),
                pa.host_port as u16,
                pa.container_port as u16,
                None,
            )
        })
        .collect();

    // Update service with first allocated port
    if let Some(first_port) = allocated_ports.first() {
        let hp = first_port.container_port;
        let sid = service_id.clone();
        with_db(&state, |db| {
            let _ = db.execute(
                "UPDATE services SET port = ?1 WHERE id = ?2",
                rusqlite::params![hp, sid],
            );
            Ok(())
        })?;
    }

    // ── Cluster deployment ───────────────────────────────────
    let is_cluster = body.deployment_mode.as_deref() == Some("cluster");

    if is_cluster {
        // Validate cluster config from catalog
        let cluster_cfg = item.cluster.as_ref().ok_or_else(|| {
            AppError::BadRequest(format!(
                "'{}' does not support cluster mode",
                body.catalog_id
            ))
        })?;

        let node_count = body.node_count.unwrap_or(cluster_cfg.default_nodes);
        if node_count < cluster_cfg.min_nodes || node_count > cluster_cfg.max_nodes {
            return Err(AppError::BadRequest(format!(
                "Node count must be between {} and {}",
                cluster_cfg.min_nodes, cluster_cfg.max_nodes
            )));
        }

        // Generate cluster compose YAML (single-server for now, all on localhost)
        let cluster_yaml = cluster_gen::build_cluster_compose(&body.catalog_id, node_count, &vars)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Cluster compose: {e}")))?;

        // Build cluster_config_json
        let nodes: Vec<serde_json::Value> = (0..node_count)
            .map(|i| {
                let role = if i == 0 { "primary" } else { "replica" };
                let server_id = body
                    .node_distribution
                    .get(i)
                    .map(|n| n.server_id.clone())
                    .unwrap_or_else(|| "localhost".to_string());
                serde_json::json!({
                    "index": i,
                    "role": role,
                    "server_id": server_id,
                })
            })
            .collect();

        let cluster_config = serde_json::json!({
            "node_count": node_count,
            "nodes": nodes,
        });

        // Update DB with cluster info
        let cluster_json = cluster_config.to_string();
        let yaml_clone = cluster_yaml.clone();
        let sid = service_id.clone();
        with_db(&state, |db| {
            let _ = db.execute(
                "UPDATE services SET cluster_mode = 'cluster', cluster_config_json = ?1, compose_content = ?2 WHERE id = ?3",
                rusqlite::params![cluster_json, yaml_clone, sid],
            );
            Ok(())
        })?;

        // Deploy cluster stack
        let deploy_result =
            docker::deploy_service_stack(&state, &service_id, &stack_name, &cluster_yaml, None)
                .await;

        let status = if deploy_result.is_ok() {
            "running"
        } else {
            "failed"
        };
        let log_output = match &deploy_result {
            Ok(out) => out.clone(),
            Err(e) => format!("{e}"),
        };
        let sid = service_id.clone();
        with_db(&state, |db| {
            let _ = db.execute(
                "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![status, sid],
            );
            record_deployment_log(db, &sid, "deploy", status, &log_output);
            Ok(())
        })?;

        deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

        // Auto-generate service domain (skip for databases — no HTTP to proxy)
        if item.meta.category != "database" {
            if let Some(http_port) = pick_http_port(&allocated_ports) {
                try_create_service_domain(&state, &service_id, &name, http_port).await;
            }
        }

        let ports_json: Vec<serde_json::Value> = allocated_ports
            .iter()
            .map(|pa| {
                serde_json::json!({
                    "name": pa.port_name,
                    "host": pa.host_port,
                    "container": pa.container_port,
                })
            })
            .collect();

        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": service_id,
            "name": name,
            "status": "running",
            "deployment_mode": "cluster",
            "node_count": node_count,
            "ports": ports_json,
        })));
    }

    // ── Standard (standalone) compose YAML ─────────────────
    let yaml = if let Some(compose) = &item.compose {
        catalog::build_from_template(&compose.template, &vars)
    } else {
        catalog::build_compose_yaml(
            &item,
            &service_id,
            &name,
            &vars,
            &port_mappings,
            network_name.as_deref(),
        )
    };

    // Resolve registry auth for this service's project (empty → None).
    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &service_id).ok())
        .unwrap_or_default();
    let deploy_auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    // Deploy (the only await point)
    let deploy_result =
        docker::deploy_service_stack(&state, &service_id, &stack_name, &yaml, deploy_auth).await;

    // Update status based on result
    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let log_output = match &deploy_result {
        Ok(out) => out.clone(),
        Err(e) => format!("{e}"),
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    with_db(&state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![status, yaml_clone, sid],
        );
        record_deployment_log(db, &sid, "deploy", status, &log_output);
        Ok(())
    })?;

    // Propagate deploy error
    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain (skip for databases — no HTTP to proxy)
    if item.meta.category != "database" {
        if let Some(http_port) = pick_http_port(&allocated_ports) {
            try_create_service_domain(&state, &service_id, &name, http_port).await;
        }
    }

    // Build response
    let ports_json: Vec<serde_json::Value> = allocated_ports
        .iter()
        .map(|pa| {
            serde_json::json!({
                "name": pa.port_name,
                "host": pa.host_port,
                "container": pa.container_port,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "running",
        "ports": ports_json,
    })))
}

/// Deploy a raw docker-compose YAML.
async fn create_compose(
    state: &SharedState,
    body: &CreateResourceRequest,
    name: &str,
    stack_name: &str,
    item: &catalog::CatalogItem,
) -> AppResult<Json<serde_json::Value>> {
    let yaml = body.config.get("yaml").cloned().unwrap_or_default();
    if yaml.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Docker Compose YAML is required".into(),
        ));
    }

    let service_id = uuid::Uuid::new_v4().to_string();
    with_db(state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, name, service_type, compose_content, status, catalog_id, category)
             VALUES (?1, ?2, ?3, 'compose', ?4, 'deploying', ?5, ?6)",
            rusqlite::params![
                service_id,
                body.project_id,
                name,
                yaml,
                body.catalog_id,
                item.meta.category,
            ],
        )?;
        Ok(())
    })?;

    let deploy_result =
        docker::deploy_service_stack(state, &service_id, stack_name, &yaml, None).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let log_output = match &deploy_result {
        Ok(out) => out.clone(),
        Err(e) => format!("{e}"),
    };
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![status, sid],
        );
        record_deployment_log(db, &sid, "deploy", status, &log_output);
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "running",
    })))
}

/// Build from a Dockerfile and deploy via compose.
async fn create_dockerfile(
    state: &SharedState,
    body: &CreateResourceRequest,
    name: &str,
    stack_name: &str,
    item: &catalog::CatalogItem,
) -> AppResult<Json<serde_json::Value>> {
    let dockerfile_content = body.config.get("dockerfile").cloned().unwrap_or_default();
    if dockerfile_content.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Dockerfile content is required".into(),
        ));
    }

    let container_port: u16 = body
        .config
        .get("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let service_id = uuid::Uuid::new_v4().to_string();

    // Allocate a host port
    let allocated_ports = with_db(state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, name, service_type, status, catalog_id, category)
             VALUES (?1, ?2, ?3, 'compose', 'deploying', ?4, ?5)",
            rusqlite::params![
                service_id,
                body.project_id,
                name,
                body.catalog_id,
                item.meta.category,
            ],
        )?;
        let port_specs = vec![("primary".to_string(), container_port)];
        Ok(ports::allocate_ports(
            db,
            &service_id,
            &port_specs,
            state.config.port_range_start,
            state.config.port_range_end,
        )?)
    })?;

    let host_port = allocated_ports
        .first()
        .map(|p| p.host_port as u16)
        .unwrap_or(container_port);

    // Update service with port
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET port = ?1 WHERE id = ?2",
            rusqlite::params![host_port as i64, sid],
        );
        Ok(())
    })?;

    // Build compose YAML that builds from Dockerfile
    let yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   build:\n\
         \x20     context: .\n\
         \x20     dockerfile: Dockerfile\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"{host_port}:{container_port}\"\n\
         \x20   restart: unless-stopped\n\
         \x20   labels:\n\
         \x20     pier.service.id: \"{service_id}\"\n\
         \x20     pier.catalog.id: \"dockerfile\"\n"
    );

    // Write Dockerfile to stack dir
    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    tokio::fs::create_dir_all(&stack_dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Create stack dir: {e}")))?;
    tokio::fs::write(stack_dir.join("Dockerfile"), &dockerfile_content)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Write Dockerfile: {e}")))?;

    // Deploy
    let deploy_result =
        docker::deploy_service_stack(state, &service_id, stack_name, &yaml, None).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let log_output = match &deploy_result {
        Ok(out) => out.clone(),
        Err(e) => format!("{e}"),
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![status, yaml_clone, sid],
        );
        record_deployment_log(db, &sid, "deploy", status, &log_output);
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain
    try_create_service_domain(state, &service_id, name, container_port as i64).await;

    let ports_json: Vec<serde_json::Value> = allocated_ports
        .iter()
        .map(|pa| {
            serde_json::json!({
                "name": pa.port_name,
                "host": pa.host_port,
                "container": pa.container_port,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "running",
        "ports": ports_json,
    })))
}

/// Clone a git repo and deploy via docker build.
async fn create_git_deploy(
    state: &SharedState,
    body: &CreateResourceRequest,
    name: &str,
    stack_name: &str,
    item: &catalog::CatalogItem,
    use_deploy_key: bool,
) -> AppResult<Json<serde_json::Value>> {
    let git_url = body.config.get("git_url").cloned().unwrap_or_default();
    if git_url.trim().is_empty() {
        return Err(AppError::BadRequest("Repository URL is required".into()));
    }

    let branch = body
        .config
        .get("branch")
        .cloned()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".to_string());

    let build_path = body
        .config
        .get("build_path")
        .cloned()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "/Dockerfile".to_string());

    let container_port: u16 = body
        .config
        .get("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let service_id = uuid::Uuid::new_v4().to_string();

    // Allocate a host port
    let allocated_ports = with_db(state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, name, service_type, status, catalog_id, category, image)
             VALUES (?1, ?2, ?3, 'compose', 'deploying', ?4, ?5, ?6)",
            rusqlite::params![
                service_id,
                body.project_id,
                name,
                body.catalog_id,
                item.meta.category,
                format!("git: {}", git_url),
            ],
        )?;
        let port_specs = vec![("primary".to_string(), container_port)];
        Ok(ports::allocate_ports(
            db,
            &service_id,
            &port_specs,
            state.config.port_range_start,
            state.config.port_range_end,
        )?)
    })?;

    let host_port = allocated_ports
        .first()
        .map(|p| p.host_port as u16)
        .unwrap_or(container_port);

    // Update service with port
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET port = ?1 WHERE id = ?2",
            rusqlite::params![host_port as i64, sid],
        );
        Ok(())
    })?;

    // Set up stack directory
    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    tokio::fs::create_dir_all(&stack_dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Create stack dir: {e}")))?;

    let repo_dir = stack_dir.join("repo");

    // If using deploy key, write it and configure SSH
    if use_deploy_key {
        let deploy_key = body.config.get("deploy_key").cloned().unwrap_or_default();
        if deploy_key.trim().is_empty() {
            return Err(AppError::BadRequest("SSH deploy key is required".into()));
        }

        let key_path = stack_dir.join("deploy_key");
        tokio::fs::write(&key_path, &deploy_key)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Write deploy key: {e}")))?;

        // chmod 600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&key_path, perms)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("chmod deploy key: {e}")))?;
        }

        // Git clone with SSH key
        let key_path_str = key_path.to_string_lossy().to_string();
        let ssh_command = format!("ssh -i {} -o StrictHostKeyChecking=no", key_path_str);

        let clone_output = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", &branch, &git_url])
            .arg(repo_dir.to_string_lossy().as_ref())
            .env("GIT_SSH_COMMAND", &ssh_command)
            .current_dir(&stack_dir)
            .output()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Git clone: {e}")))?;

        if !clone_output.status.success() {
            let stderr = String::from_utf8_lossy(&clone_output.stderr);
            // Update status to failed
            let sid = service_id.clone();
            with_db(state, |db| {
                let _ = db.execute(
                    "UPDATE services SET status = 'failed', updated_at = datetime('now') WHERE id = ?1",
                    rusqlite::params![sid],
                );
                Ok(())
            })?;
            return Err(AppError::Internal(anyhow::anyhow!(
                "Git clone failed: {stderr}"
            )));
        }
    } else {
        // Public repo — simple git clone
        let clone_output = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", "--branch", &branch, &git_url])
            .arg(repo_dir.to_string_lossy().as_ref())
            .current_dir(&stack_dir)
            .output()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Git clone: {e}")))?;

        if !clone_output.status.success() {
            let stderr = String::from_utf8_lossy(&clone_output.stderr);
            let sid = service_id.clone();
            with_db(state, |db| {
                let _ = db.execute(
                    "UPDATE services SET status = 'failed', updated_at = datetime('now') WHERE id = ?1",
                    rusqlite::params![sid],
                );
                Ok(())
            })?;
            return Err(AppError::Internal(anyhow::anyhow!(
                "Git clone failed: {stderr}"
            )));
        }
    }

    // Determine Dockerfile path relative to repo root
    let dockerfile_rel = build_path.trim_start_matches('/');
    let dockerfile_in_repo = if dockerfile_rel.is_empty() {
        "Dockerfile".to_string()
    } else {
        dockerfile_rel.to_string()
    };

    // Verify Dockerfile exists
    let full_dockerfile_path = repo_dir.join(&dockerfile_in_repo);
    if !full_dockerfile_path.exists() {
        let sid = service_id.clone();
        with_db(state, |db| {
            let _ = db.execute(
                "UPDATE services SET status = 'failed', updated_at = datetime('now') WHERE id = ?1",
                rusqlite::params![sid],
            );
            Ok(())
        })?;
        return Err(AppError::BadRequest(format!(
            "Dockerfile not found at '{}' in repository",
            dockerfile_in_repo
        )));
    }

    // Build context = directory containing the Dockerfile
    // e.g. "mainline/debian/Dockerfile" → context "./repo/mainline/debian", dockerfile "Dockerfile"
    let dockerfile_path = std::path::Path::new(&dockerfile_in_repo);
    let (build_context, dockerfile_name) = if let Some(parent) = dockerfile_path.parent() {
        let parent_str = parent.to_string_lossy();
        if parent_str.is_empty() || parent_str == "." {
            ("./repo".to_string(), dockerfile_in_repo.clone())
        } else {
            (
                format!("./repo/{}", parent_str.replace('\\', "/")),
                dockerfile_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
            )
        }
    } else {
        ("./repo".to_string(), dockerfile_in_repo.clone())
    };

    // Build compose YAML pointing to the cloned repo
    let yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   build:\n\
         \x20     context: {build_context}\n\
         \x20     dockerfile: {dockerfile_name}\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"{host_port}:{container_port}\"\n\
         \x20   restart: unless-stopped\n\
         \x20   labels:\n\
         \x20     pier.service.id: \"{service_id}\"\n\
         \x20     pier.catalog.id: \"{catalog_id}\"\n",
        catalog_id = body.catalog_id,
    );

    // Deploy
    let deploy_result =
        docker::deploy_service_stack(state, &service_id, stack_name, &yaml, None).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let log_output = match &deploy_result {
        Ok(out) => out.clone(),
        Err(e) => format!("{e}"),
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    let env_data = serde_json::json!({
        "GIT_URL": git_url,
        "GIT_BRANCH": branch,
        "DOCKERFILE_PATH": build_path,
    });
    let env_json_stored = crate::crypto::encrypt_env_json(&env_data.to_string());
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, env_json = ?3, updated_at = datetime('now') WHERE id = ?4",
            rusqlite::params![status, yaml_clone, env_json_stored, sid],
        );
        record_deployment_log(db, &sid, "deploy", status, &log_output);
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain
    if let Some(http_port) = pick_http_port(&allocated_ports) {
        try_create_service_domain(state, &service_id, name, http_port).await;
    }

    let ports_json: Vec<serde_json::Value> = allocated_ports
        .iter()
        .map(|pa| {
            serde_json::json!({
                "name": pa.port_name,
                "host": pa.host_port,
                "container": pa.container_port,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "running",
        "ports": ports_json,
    })))
}

/// Deploy from a GitHub App source.
/// Fetches an installation token and clones via HTTPS with token injection.
async fn create_git_deploy_github_app(
    state: &SharedState,
    body: &CreateResourceRequest,
    name: &str,
    _stack_name: &str,
    item: &catalog::CatalogItem,
) -> AppResult<Json<serde_json::Value>> {
    let source_id = body.source_id.as_deref().ok_or_else(|| {
        AppError::BadRequest("source_id is required for GitHub App deploy".into())
    })?;

    // Load source credentials from DB
    let (app_id, installation_id, private_key) = with_db(state, |db| {
        let row = db.query_row(
            "SELECT app_id, installation_id, private_key FROM git_sources WHERE id = ?1 AND source_type = 'github-app'",
            [source_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        ).map_err(|_| AppError::NotFound(format!("GitHub App source {source_id} not found")))?;
        let app_id = row
            .0
            .ok_or_else(|| AppError::BadRequest("Source missing app_id".into()))?;
        let inst_id = row
            .1
            .ok_or_else(|| AppError::BadRequest("Source missing installation_id".into()))?;
        let pk = row
            .2
            .ok_or_else(|| AppError::BadRequest("Source missing private_key".into()))?;
        Ok((app_id, inst_id, pk))
    })?;

    // Get installation access token
    let token =
        crate::git::github_app::get_installation_token(&app_id, installation_id, &private_key)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("GitHub App token: {e}")))?;

    let git_url = body.config.get("git_url").cloned().unwrap_or_default();
    if git_url.trim().is_empty() {
        return Err(AppError::BadRequest("Repository URL is required".into()));
    }

    // Inject token into clone URL: https://x-access-token:{token}@github.com/owner/repo.git
    let _clone_url = if git_url.starts_with("https://") {
        git_url.replacen("https://", &format!("https://x-access-token:{token}@"), 1)
    } else {
        return Err(AppError::BadRequest("GitHub App requires HTTPS URL".into()));
    };

    let branch = body
        .config
        .get("branch")
        .cloned()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".to_string());
    let _build_path = body
        .config
        .get("build_path")
        .cloned()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "/Dockerfile".to_string());
    let _compose_path = body
        .config
        .get("compose_path")
        .cloned()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "/docker-compose.yml".to_string());
    let build_pack = body
        .config
        .get("build_pack")
        .cloned()
        .unwrap_or_else(|| "dockerfile".to_string());
    let container_port: u16 = body
        .config
        .get("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let build_strategy = if build_pack == "docker-compose" {
        "docker-compose"
    } else {
        "dockerfile"
    };

    let service_id = uuid::Uuid::new_v4().to_string();

    // Resolve network_id
    let network_id = body.network_id.clone().or_else(|| {
        state.db.lock().ok().and_then(|db| {
            db.query_row(
                "SELECT id FROM networks WHERE is_default = 1 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok()
        })
    });

    // Create service record WITHOUT deploying (status = "created")
    let allocated_ports = with_db(state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, network_id, name, service_type, status, catalog_id, category, image, git_repo_url, git_branch, git_source_id, build_strategy)
             VALUES (?1, ?2, ?3, ?4, 'compose', 'created', ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                service_id, body.project_id, network_id, name, body.catalog_id, item.meta.category,
                format!("git: {}", git_url), git_url, branch, source_id, build_strategy
            ],
        )?;
        let port_specs = vec![("primary".to_string(), container_port)];
        Ok(ports::allocate_ports(
            db,
            &service_id,
            &port_specs,
            state.config.port_range_start,
            state.config.port_range_end,
        )?)
    })?;

    let host_port = allocated_ports
        .first()
        .map(|p| p.host_port as u16)
        .unwrap_or(container_port);

    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET port = ?1 WHERE id = ?2",
            rusqlite::params![host_port as i64, sid],
        );
        Ok(())
    })?;

    let ports_json: Vec<serde_json::Value> = allocated_ports
        .iter()
        .map(|pa| serde_json::json!({"name": pa.port_name, "host": pa.host_port, "container": pa.container_port}))
        .collect();

    tracing::info!("Created git resource '{name}' (no auto-deploy, status=created)");

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "created",
        "ports": ports_json,
    })))
}

/// GET /api/v1/resources — list deployed resources visible to the caller.
///
/// Global Admin+ and peer requests see every resource. Plain Users see only
/// resources whose `project_id` matches a project they're a member of.
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let see_all = user.is_peer || user.global_role.at_least(GlobalRole::Admin);

    let row_to_json = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
        let catalog_id: Option<String> = row.get(7)?;
        let icon: Option<String> = catalog_id.as_deref().and_then(|cid| {
            state
                .catalog
                .iter()
                .find(|i| i.meta.id == cid)
                .and_then(|i| i.meta.icon.clone())
        });
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "project_id": row.get::<_, Option<String>>(1)?,
            "name": row.get::<_, String>(2)?,
            "service_type": row.get::<_, String>(3)?,
            "status": row.get::<_, String>(4)?,
            "port": row.get::<_, Option<i64>>(5)?,
            "image": row.get::<_, Option<String>>(6)?,
            "catalog_id": catalog_id,
            "category": row.get::<_, Option<String>>(8)?,
            "created_at": row.get::<_, String>(9)?,
            "git_repo_url": row.get::<_, Option<String>>(10)?,
            "primary_domain": row.get::<_, Option<String>>(11)?,
            "icon": icon,
        }))
    };

    let resources: Vec<serde_json::Value> = if see_all {
        let mut stmt = db.prepare(
            "SELECT s.id, s.project_id, s.name, s.service_type, s.status, s.port, s.image,
                    s.catalog_id, s.category, s.created_at, s.git_repo_url,
                    (SELECT domain FROM domains WHERE service_id = s.id ORDER BY created_at LIMIT 1) AS primary_domain
             FROM services s WHERE s.catalog_id IS NOT NULL ORDER BY s.created_at DESC",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], row_to_json)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        let mut stmt = db.prepare(
            "SELECT s.id, s.project_id, s.name, s.service_type, s.status, s.port, s.image,
                    s.catalog_id, s.category, s.created_at, s.git_repo_url,
                    (SELECT domain FROM domains WHERE service_id = s.id ORDER BY created_at LIMIT 1) AS primary_domain
             FROM services s
             JOIN project_members pm ON pm.project_id = s.project_id
             WHERE s.catalog_id IS NOT NULL AND pm.user_id = ?1
             ORDER BY s.created_at DESC",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([&user.id], row_to_json)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    Ok(Json(resources))
}

/// GET /api/v1/resources/{id} — get resource details with ports.
pub async fn get(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let resource = db
        .query_row(
            "SELECT s.id, s.project_id, s.name, s.service_type, s.status, s.port, s.image, s.catalog_id, s.category, s.env_json, s.compose_content, s.created_at, s.cluster_mode, s.cluster_config_json, s.network_id, n.name, s.auto_deploy, s.force_https, s.container_id, s.env_dirty
             FROM services s LEFT JOIN networks n ON s.network_id = n.id WHERE s.id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "project_id": row.get::<_, Option<String>>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "service_type": row.get::<_, String>(3)?,
                    "status": row.get::<_, String>(4)?,
                    "port": row.get::<_, Option<i64>>(5)?,
                    "image": row.get::<_, Option<String>>(6)?,
                    "catalog_id": row.get::<_, Option<String>>(7)?,
                    "category": row.get::<_, Option<String>>(8)?,
                    "env_json": serde_json::Value::Null, // SEC-003: secrets not exposed in detail API
                    "compose_content": row.get::<_, Option<String>>(10)?,
                    "created_at": row.get::<_, String>(11)?,
                    "cluster_mode": row.get::<_, Option<String>>(12)?,
                    "cluster_config_json": row.get::<_, Option<String>>(13)?,
                    "network_id": row.get::<_, Option<String>>(14)?,
                    "network_name": row.get::<_, Option<String>>(15)?,
                    "auto_deploy": row.get::<_, Option<bool>>(16)?.unwrap_or(true),
                    "force_https": row.get::<_, Option<bool>>(17)?.unwrap_or(true),
                    "stored_container_name": row.get::<_, Option<String>>(18)?,
                    "env_dirty": row.get::<_, i64>(19)? == 1,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?;

    let port_allocs = ports::get_ports(&db, &id)?;
    let ports_json: Vec<serde_json::Value> = port_allocs
        .iter()
        .map(|pa| {
            serde_json::json!({
                "id": pa.id,
                "name": pa.port_name,
                "host": pa.host_port,
                "container": pa.container_port,
                "protocol": pa.protocol,
                "is_public": pa.is_public,
                "public_port": pa.public_port,
                "compose_service": pa.compose_service,
            })
        })
        .collect();

    // Get public IP for connection URLs
    let public_ip = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'server.public_ip'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    // Get container name: stored DB value → parse from compose YAML → fallback pier-{name}
    let svc_name = resource["name"].as_str().unwrap_or_default();
    let default_name = format!("pier-{}", svc_name.to_lowercase().replace(' ', "-"));
    let container_name = resource["stored_container_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Try to parse container_name from compose YAML on disk
            let compose_path = state
                .config
                .data_dir
                .join("stacks")
                .join(&default_name)
                .join("docker-compose.yml");
            if let Ok(yaml) = std::fs::read_to_string(&compose_path) {
                yaml.lines()
                    .find_map(|l| {
                        let t = l.trim();
                        t.strip_prefix("container_name:")
                            .map(|n| n.trim().trim_matches('"').trim_matches('\'').to_string())
                    })
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| default_name.clone())
            } else {
                default_name.clone()
            }
        });

    let mut result = resource;
    result["ports"] = serde_json::json!(ports_json);
    result["public_ip"] = serde_json::json!(public_ip);
    result["container_name"] = serde_json::json!(container_name);
    Ok(Json(result))
}

/// Best-effort deletion of S3 backup blobs for the given `(s3_storage_id, s3_key)`
/// pairs. Loads each distinct storage's credentials once, then issues a
/// `delete_blob` per key. Failures are logged but never propagated — orphan
/// blobs are recoverable, a half-finished service delete is not.
pub(crate) async fn purge_backup_blobs(state: &SharedState, blobs: &[(String, String)]) {
    use std::collections::HashMap as StdMap;

    let storage_ids: std::collections::HashSet<&str> =
        blobs.iter().map(|(sid, _)| sid.as_str()).collect();
    let mut storages: StdMap<String, (String, String, String, String, String, String)> =
        StdMap::new();
    {
        let db = match state.db.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("S3 cleanup: DB lock poisoned: {e}");
                return;
            }
        };
        for sid in storage_ids {
            let row = db.query_row(
                "SELECT storage_type, endpoint, region, bucket, access_key, secret_key
                 FROM s3_storages WHERE id = ?1",
                [sid],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            );
            match row {
                Ok(t) => {
                    storages.insert(sid.to_string(), t);
                }
                Err(e) => tracing::warn!("S3 cleanup: storage {sid} lookup failed: {e}"),
            }
        }
    }

    for (sid, key) in blobs {
        let Some((storage_type, endpoint, region, bucket, access_key, secret_key)) =
            storages.get(sid)
        else {
            continue;
        };
        if let Err(e) = crate::s3::delete_blob(
            storage_type,
            endpoint,
            region,
            bucket,
            access_key,
            secret_key,
            key,
        )
        .await
        {
            tracing::warn!("S3 cleanup: delete {key} from storage {sid} failed: {e}");
        }
    }
}

/// DELETE /api/v1/resources/{id} — stop and remove a resource.
///
/// Refuses to proceed if the service still owns user-created databases
/// (tracked in `database_credentials`); the user must drop those through the
/// per-database endpoint first. Optional `?delete_backups=true` enumerates
/// the service's S3-stored backups and removes the blobs before the
/// `backups.service_id ON DELETE CASCADE` wipes the SQLite rows — otherwise
/// the blob keys would be lost and the S3 objects would orphan.
pub async fn remove(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    Json(body): Json<DeleteRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row("SELECT name FROM services WHERE id = ?1", [&id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let delete_volumes = params
        .get("delete_volumes")
        .map(|v| v == "true")
        .unwrap_or(false);
    let delete_backups = params
        .get("delete_backups")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Block deletion while user databases live inside this service.
    // Tracked via Pier UI — manually created DBs that bypassed the UI are
    // not Pier's concern.
    let backup_blobs: Vec<(String, String)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let user_db_count: i64 = db.query_row(
            "SELECT COUNT(*) FROM database_credentials WHERE service_id = ?1",
            [&id],
            |row| row.get(0),
        )?;
        if user_db_count > 0 {
            return Err(AppError::Conflict(format!(
                "Service has {user_db_count} database(s). Delete them first."
            )));
        }

        security::verify_delete_password(&db, &user.id, body.password.as_deref())?;

        if delete_backups {
            let mut stmt =
                db.prepare("SELECT s3_storage_id, s3_key FROM backups WHERE service_id = ?1")?;
            let collected: Vec<(String, String)> = stmt
                .query_map([&id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect();
            collected
        } else {
            Vec::new()
        }
    };

    if delete_backups && !backup_blobs.is_empty() {
        // Best-effort: log failures and keep going. The SQL CASCADE below
        // will still drop the rows; we'd rather end up with a few orphan
        // blobs than half a deletion.
        purge_backup_blobs(&state, &backup_blobs).await;
    }

    tracing::info!(
        "Deleting resource '{name}' (id={id}, stack={stack_name}, delete_volumes={delete_volumes}, delete_backups={delete_backups})"
    );

    // Stop containers (with or without volumes)
    if delete_volumes {
        // docker compose down -v (removes named volumes)
        let result = docker::compose::down_stack_with_volumes(&stack_name, &state.config).await;
        match &result {
            Ok(out) => tracing::info!("Stack {stack_name} down -v: {out}"),
            Err(e) => tracing::warn!("Stack {stack_name} down -v failed: {e}"),
        }
    } else {
        let result = docker::compose::down_stack(&stack_name, &state.config).await;
        match &result {
            Ok(out) => tracing::info!("Stack {stack_name} down: {out}"),
            Err(e) => tracing::warn!("Stack {stack_name} down failed: {e}"),
        }
    }

    // Remove stack directory
    match docker::compose::remove_stack(&stack_name, &state.config).await {
        Ok(()) => tracing::info!("Stack {stack_name} directory removed"),
        Err(e) => tracing::warn!("Stack {stack_name} dir remove failed: {e}"),
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    ports::free_ports(&db, &id)?;

    // Clean up related records
    let _ = db.execute("DELETE FROM canvas_positions WHERE service_id = ?1", [&id]);
    let _ = db.execute("DELETE FROM port_allocations WHERE service_id = ?1", [&id]);

    // Remove Traefik dynamic configs for domains and TCP proxies
    let domain_ids: Vec<String> = db
        .prepare("SELECT id FROM domains WHERE service_id = ?1")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([&id], |row| row.get(0))
                .unwrap_or_else(|_| panic!())
                .filter_map(|r| r.ok())
                .collect()
        })
        .unwrap_or_default();
    for did in &domain_ids {
        let config_path = state
            .config
            .data_dir
            .join("traefik")
            .join("dynamic")
            .join(format!("{did}.yml"));
        let _ = std::fs::remove_file(&config_path);
    }
    let _ = db.execute("DELETE FROM domains WHERE service_id = ?1", [&id]);

    // Remove TCP proxy configs
    let tcp_config = state
        .config
        .data_dir
        .join("traefik")
        .join("dynamic")
        .join(format!("tcp-{id}.yml"));
    let _ = std::fs::remove_file(&tcp_config);

    db.execute("DELETE FROM services WHERE id = ?1", [&id])?;

    tracing::info!("Resource '{name}' ({id}) fully removed");
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/resources/{id}/stop
pub async fn stop(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row("SELECT name FROM services WHERE id = ?1", [&id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let result = docker::compose::down_stack(&stack_name, &state.config).await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let status_str = if result.is_ok() { "success" } else { "failed" };
    let log_output = match &result {
        Ok(o) => o.clone(),
        Err(e) => format!("{e}"),
    };
    record_deployment_log(&db, &id, "stop", status_str, &log_output);
    let _ = db.execute(
        "UPDATE services SET status = 'stopped', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    result?;
    Ok(Json(serde_json::json!({"ok": true, "status": "stopped"})))
}

/// POST /api/v1/resources/{id}/start
pub async fn start(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, compose_content FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let yaml = yaml.ok_or_else(|| AppError::BadRequest("No compose content found".into()))?;
    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    let result = docker::deploy_service_stack(&state, &id, &stack_name, &yaml, None).await;

    let status = if result.is_ok() { "running" } else { "failed" };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let log_output = match &result {
        Ok(o) => o.clone(),
        Err(e) => format!("{e}"),
    };
    record_deployment_log(
        &db,
        &id,
        "start",
        if result.is_ok() { "success" } else { "failed" },
        &log_output,
    );
    let _ = db.execute(
        "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![status, id],
    );

    result?;
    Ok(Json(serde_json::json!({"ok": true, "status": "running"})))
}

/// POST /api/v1/resources/{id}/restart
pub async fn restart(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, compose_content FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let yaml = yaml.ok_or_else(|| AppError::BadRequest("No compose content found".into()))?;
    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    let _ = docker::compose::down_stack(&stack_name, &state.config).await;
    let result = docker::deploy_service_stack(&state, &id, &stack_name, &yaml, None).await;

    let status = if result.is_ok() { "running" } else { "failed" };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let log_output = match &result {
        Ok(o) => o.clone(),
        Err(e) => format!("{e}"),
    };
    record_deployment_log(
        &db,
        &id,
        "restart",
        if result.is_ok() { "success" } else { "failed" },
        &log_output,
    );
    let _ = db.execute(
        "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![status, id],
    );

    result?;
    Ok(Json(serde_json::json!({"ok": true, "status": "running"})))
}

/// POST /api/v1/resources/{id}/redeploy
pub async fn redeploy(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let no_cache = params.get("no_cache").map(|v| v == "true").unwrap_or(false);
    let (name, yaml, git_repo_url, git_branch) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, compose_content, git_repo_url, git_branch FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    // Git-based services: run full pipeline (clone + build + deploy)
    if let Some(repo_url) = &git_repo_url {
        if !repo_url.is_empty() {
            let branch = git_branch.unwrap_or_else(|| "main".to_string());
            let commit = crate::deploy::CommitInfo {
                sha: "redeploy".to_string(),
                message: "Redeploy".to_string(),
                branch: branch.clone(),
            };
            // Set deploying status
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                let _ = db.execute("UPDATE services SET status = 'deploying', updated_at = datetime('now') WHERE id = ?1", [&id]);
            }
            let state_clone = std::sync::Arc::clone(&state);
            let sid = id.clone();
            tokio::spawn(async move {
                crate::deploy::run_pipeline(state_clone, sid, commit, "redeploy").await;
            });
            return Ok(Json(
                serde_json::json!({"ok": true, "message": "Redeploy pipeline started"}),
            ));
        }
    }

    // Catalog-based services: use saved compose YAML
    let yaml = yaml.ok_or_else(|| AppError::BadRequest("No compose content found".into()))?;
    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    // Stop existing stack
    let _ = docker::compose::down_stack(&stack_name, &state.config).await;

    // Set status to deploying
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE services SET status = 'deploying', updated_at = datetime('now') WHERE id = ?1",
            [&id],
        );
    }

    // Registry auth scoped to this service's project.
    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let redeploy_auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    // Redeploy (with optional --no-cache for force deploy)
    let result = if no_cache {
        docker::deploy_service_stack_no_cache(&state, &id, &stack_name, &yaml, redeploy_auth).await
    } else {
        docker::deploy_service_stack(&state, &id, &stack_name, &yaml, redeploy_auth).await
    };

    let status = if result.is_ok() { "running" } else { "failed" };
    let log_output = match &result {
        Ok(o) => o.clone(),
        Err(e) => format!("{e}"),
    };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    record_deployment_log(
        &db,
        &id,
        "redeploy",
        if result.is_ok() { "success" } else { "failed" },
        &log_output,
    );
    let _ = db.execute(
        "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![status, id],
    );

    result?;
    Ok(Json(serde_json::json!({"ok": true, "status": "running"})))
}

/// GET /api/v1/resources/{id}/nodes — get cluster node info
pub async fn get_nodes(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let (cluster_mode, cluster_config_json, catalog_id) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT cluster_mode, cluster_config_json, catalog_id FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    if cluster_mode.as_deref() != Some("cluster") {
        return Ok(Json(serde_json::json!({
            "cluster": false,
            "nodes": [],
        })));
    }

    let config: serde_json::Value = cluster_config_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::json!({}));

    Ok(Json(serde_json::json!({
        "cluster": true,
        "catalog_id": catalog_id,
        "node_count": config["node_count"],
        "nodes": config["nodes"],
    })))
}

/// POST /api/v1/resources/{id}/scale — scale cluster up/down
pub async fn scale(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ScaleRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let (name, catalog_id, cluster_mode, cluster_config_json, env_json) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, cluster_mode, cluster_config_json, env_json FROM services WHERE id = ?1",
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

    if cluster_mode.as_deref() != Some("cluster") {
        return Err(AppError::BadRequest("Resource is not a cluster".into()));
    }

    let catalog_id =
        catalog_id.ok_or_else(|| AppError::Internal(anyhow::anyhow!("No catalog_id")))?;

    // Validate against catalog limits
    let cluster_cfg = state
        .catalog
        .iter()
        .find(|i| i.meta.id == catalog_id)
        .and_then(|i| i.cluster.as_ref())
        .ok_or_else(|| AppError::BadRequest("Catalog not found or no cluster support".into()))?;

    if body.node_count < cluster_cfg.min_nodes || body.node_count > cluster_cfg.max_nodes {
        return Err(AppError::BadRequest(format!(
            "Node count must be between {} and {}",
            cluster_cfg.min_nodes, cluster_cfg.max_nodes
        )));
    }

    // Parse existing vars (may be encrypted — decrypt_env_json handles both)
    let decrypted = crate::crypto::decrypt_env_json(env_json.as_deref());
    let vars: HashMap<String, String> = serde_json::from_str(&decrypted).unwrap_or_default();

    // Generate new compose with updated node count
    let new_yaml = cluster_gen::build_cluster_compose(&catalog_id, body.node_count, &vars)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Cluster compose: {e}")))?;

    // Parse old config, update nodes
    let mut config: serde_json::Value = cluster_config_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::json!({}));

    let old_count = config["node_count"].as_u64().unwrap_or(0) as usize;

    // Build new nodes list
    let mut nodes: Vec<serde_json::Value> = Vec::new();
    for i in 0..body.node_count {
        let role = if i == 0 { "primary" } else { "replica" };
        let server_id = if i < old_count {
            config["nodes"]
                .get(i)
                .and_then(|n| n["server_id"].as_str())
                .unwrap_or("localhost")
                .to_string()
        } else {
            body.server_id
                .clone()
                .unwrap_or_else(|| "localhost".to_string())
        };
        nodes.push(serde_json::json!({
            "index": i,
            "role": role,
            "server_id": server_id,
        }));
    }
    config["node_count"] = serde_json::json!(body.node_count);
    config["nodes"] = serde_json::json!(nodes);

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    // Redeploy with new compose
    let _ = docker::compose::down_stack(&stack_name, &state.config).await;
    let deploy_result =
        docker::deploy_service_stack(&state, &id, &stack_name, &new_yaml, None).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };

    let cluster_json = config.to_string();
    let yaml_clone = new_yaml.clone();
    with_db(&state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, cluster_config_json = ?3, updated_at = datetime('now') WHERE id = ?4",
            rusqlite::params![status, yaml_clone, cluster_json, id],
        );
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Scale failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "node_count": body.node_count,
        "status": "running",
    })))
}

// ── Load Balancing (Phase 11.2) ─────────────────────────────────────

const LB_MAX_REPLICAS_PER_SERVICE: i64 = 10;
const LB_PORT_RANGE_START: u16 = 10000;
const LB_PORT_RANGE_END: u16 = 20000;

/// GET /api/v1/resources/{id}/load-balance — current LB config + distribution.
pub async fn get_load_balance(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (replicas, lb_strategy, sticky_cookie, cluster_mode): (
        i64,
        String,
        Option<String>,
        Option<String>,
    ) = db
        .query_row(
            "SELECT replicas, lb_strategy, lb_sticky_cookie, cluster_mode
             FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?;

    // Canonical local server (bootstrap guarantees one row with is_local=1).
    // Used to resolve NULL/empty server_id and any server_id that no longer
    // exists in `servers` (e.g., legacy service_replicas backfill).
    let local_srv: Option<(String, String)> = db
        .query_row(
            "SELECT id, name FROM servers WHERE is_local = 1 ORDER BY created_at ASC LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();

    let mut stmt = db.prepare(
        "SELECT r.server_id, r.replica_idx, r.host_port, r.status, r.weight, s.name
         FROM service_replicas r
         LEFT JOIN servers s ON s.id = r.server_id
         WHERE r.service_id = ?1
         ORDER BY r.server_id, r.replica_idx",
    )?;
    // `(server_id?, replica_idx, host_port, status, weight, server_name?)`
    type ReplicaRow = (Option<String>, i64, i64, String, i64, Option<String>);
    let rows: Vec<ReplicaRow> = stmt
        .query_map([&id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut by_server: std::collections::BTreeMap<String, (String, i64, Vec<serde_json::Value>)> =
        std::collections::BTreeMap::new();
    for (server_id, idx, host_port, status, weight, server_name) in rows {
        // Fall back to the canonical local server id/name when the replica
        // row has NULL/empty server_id or points at a now-deleted server.
        let raw = server_id.unwrap_or_default();
        let (key, display_name) = if let Some(name) = server_name.filter(|_| !raw.is_empty()) {
            (raw, name)
        } else if let Some((lid, lname)) = local_srv.clone() {
            (lid, lname)
        } else {
            (raw, "local".to_string())
        };
        let entry = by_server
            .entry(key)
            .or_insert_with(|| (display_name, weight, Vec::new()));
        entry.2.push(serde_json::json!({
            "idx": idx,
            "host_port": host_port,
            "status": status,
        }));
    }
    let distribution: Vec<serde_json::Value> = by_server
        .into_iter()
        .map(|(server_id, (server_name, weight, replicas))| {
            serde_json::json!({
                "server_id": server_id,
                "server_name": server_name,
                "weight": weight,
                "replicas": replicas,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "replicas": replicas,
        "strategy": lb_strategy,
        "sticky_cookie": sticky_cookie,
        "cluster_mode": cluster_mode,
        "distribution": distribution,
    })))
}

/// POST /api/v1/resources/{id}/load-balance — apply a new LB plan.
///
/// Body: see `LoadBalanceRequest`. Redeploys compose per affected server,
/// rewrites `service_replicas`, and regenerates Traefik dynamic config.
pub async fn load_balance(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<LoadBalanceRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    // ── Step 1. Read service row ──────────────────────────────────
    let (name, catalog_id, env_json, network_id, current_server_id, cluster_mode) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, env_json, network_id, server_id, cluster_mode
             FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    if cluster_mode.as_deref() == Some("cluster") {
        return Err(AppError::Conflict(
            "Cluster-mode services use /scale instead".into(),
        ));
    }

    let catalog_id = catalog_id.ok_or_else(|| {
        AppError::BadRequest("Resource has no catalog_id; LB not supported".into())
    })?;

    let item = state
        .catalog
        .iter()
        .find(|i| i.meta.id == catalog_id)
        .ok_or_else(|| AppError::BadRequest(format!("Catalog template '{catalog_id}' not found")))?
        .clone();

    // Reject volume-owning templates (v1: risk of corruption with N writers).
    if !item.volumes.is_empty() {
        return Err(AppError::Conflict(
            "Catalog item has named volumes; multi-replica scaling is disabled in v1".into(),
        ));
    }

    let docker = item
        .docker
        .as_ref()
        .ok_or_else(|| AppError::BadRequest("Catalog has no docker section".into()))?;

    // Pick primary container port (first catalog port, deterministic order).
    let mut port_entries: Vec<(&String, &crate::catalog::PortConfig)> = item.ports.iter().collect();
    port_entries.sort_by_key(|(k, _)| (*k).clone());
    let (primary_port_name, primary_port) = port_entries
        .first()
        .map(|(k, v)| ((*k).clone(), v.internal))
        .ok_or_else(|| AppError::BadRequest("Catalog has no ports; LB not applicable".into()))?;

    // ── Step 2. Parse & normalize request ─────────────────────────
    let strategy_str = body
        .strategy
        .as_deref()
        .unwrap_or("round-robin")
        .to_string();
    let strategy = match strategy_str.as_str() {
        "round-robin" => crate::proxy::config::LbStrategy::RoundRobin,
        "weighted" => crate::proxy::config::LbStrategy::Weighted,
        "sticky" => crate::proxy::config::LbStrategy::Sticky,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown lb_strategy '{other}' (expected round-robin|weighted|sticky)"
            )));
        }
    };
    let sticky_cookie = if strategy == crate::proxy::config::LbStrategy::Sticky {
        let c = body
            .sticky_cookie
            .clone()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| "PIER_SESSION".to_string());
        Some(c)
    } else {
        None
    };

    let mut distribution: Vec<LoadBalanceSlot> = if body.distribution.is_empty() {
        let total = body.replicas.unwrap_or(1);
        if total < 1 {
            return Err(AppError::BadRequest("replicas must be >= 1".into()));
        }
        // Resolve fallback server: services.server_id if it exists, otherwise
        // the canonical local server row. Never use the literal "localhost"
        // — the servers table uses `id='local'`, not `localhost`.
        let server_id = match current_server_id.clone().filter(|s| !s.is_empty()) {
            Some(id) => id,
            None => {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.query_row(
                    "SELECT id FROM servers WHERE is_local = 1 ORDER BY created_at ASC LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| {
                    AppError::Internal(anyhow::anyhow!(
                        "No local server found; cannot resolve default server_id"
                    ))
                })?
            }
        };
        vec![LoadBalanceSlot {
            server_id,
            replicas: total,
            weight: 1,
        }]
    } else {
        body.distribution.clone()
    };

    // Drop empty slots and validate
    distribution.retain(|s| s.replicas > 0);
    if distribution.is_empty() {
        return Err(AppError::BadRequest("No replicas requested".into()));
    }
    let total_replicas: i64 = distribution.iter().map(|s| s.replicas).sum();
    if total_replicas > LB_MAX_REPLICAS_PER_SERVICE {
        return Err(AppError::BadRequest(format!(
            "Max {LB_MAX_REPLICAS_PER_SERVICE} replicas per service"
        )));
    }

    // Resolve server info once per unique server_id
    let mut server_info: std::collections::HashMap<String, (String, i64, String, bool)> =
        std::collections::HashMap::new();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        for slot in &distribution {
            if server_info.contains_key(&slot.server_id) {
                continue;
            }
            let info = db
                .query_row(
                    "SELECT host, port, agent_token, is_local FROM servers WHERE id = ?1",
                    [&slot.server_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, bool>(3)?,
                        ))
                    },
                )
                .map_err(|_| {
                    AppError::BadRequest(format!("Server '{}' not found", slot.server_id))
                })?;
            server_info.insert(slot.server_id.clone(), info);
        }
    }

    // ── Step 3. Reset port allocations + insert N replicas ────────
    let env_vars: HashMap<String, String> = {
        let decrypted = crate::crypto::decrypt_env_json(env_json.as_deref());
        serde_json::from_str(&decrypted).unwrap_or_default()
    };

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let network_name = if let Some(net_id) = &network_id {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row("SELECT name FROM networks WHERE id = ?1", [net_id], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    } else {
        None
    };

    let image = crate::catalog::substitute(&docker.image, &env_vars);

    // Allocate N fresh host ports for all replicas across all servers
    let port_specs: Vec<(String, u16)> = (1..=total_replicas)
        .map(|i| (format!("replica_{i}"), primary_port))
        .collect();

    // Snapshot the public-port state so we can restore it on the fresh
    // allocations; `free_ports` below would otherwise drop the is_public
    // + public_port flags entirely.
    let public_snapshot: Option<i64> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT public_port FROM port_allocations
             WHERE service_id = ?1 AND is_public = 1 AND public_port IS NOT NULL
             LIMIT 1",
            [&id],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    };

    let allocated_ports: Vec<i64> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        crate::db::ports::free_ports(&db, &id)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("free_ports: {e}")))?;
        let allocs = crate::db::ports::allocate_ports(
            &db,
            &id,
            &port_specs,
            LB_PORT_RANGE_START,
            LB_PORT_RANGE_END,
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("allocate_ports: {e}")))?;
        // Re-apply public flag so the user's toggle survives a scale.
        if let Some(pub_port) = public_snapshot {
            let _ = db.execute(
                "UPDATE port_allocations SET is_public = 1, public_port = ?1
                 WHERE service_id = ?2",
                rusqlite::params![pub_port, id],
            );
        }
        allocs.iter().map(|a| a.host_port).collect()
    };

    // Assign ports to slots: first slot.replicas ports → slot 1, next → slot 2, …
    let mut cursor = 0usize;
    let mut per_server_plan: Vec<(LoadBalanceSlot, Vec<(i64, u16)>)> = Vec::new();
    for slot in &distribution {
        let mut replicas_for_slot: Vec<(i64, u16)> = Vec::new();
        for local_idx in 1..=slot.replicas {
            let host_port = allocated_ports[cursor] as u16;
            cursor += 1;
            replicas_for_slot.push((local_idx, host_port));
        }
        per_server_plan.push((slot.clone(), replicas_for_slot));
    }

    // ── Step 4. Build compose per server, then deploy ──────────────
    let mut deploy_errors: Vec<String> = Vec::new();
    let primary_public = public_snapshot.map(|p| p as u16);
    // The kernel won't allow two containers to bind the same host port, so the
    // public 0.0.0.0 mapping goes to the very first local replica only. HA
    // across replicas for raw TCP needs a real LB, which is out of scope.
    let mut public_pending = primary_public;
    for (slot_idx, (slot, replicas_for_server)) in per_server_plan.iter().enumerate() {
        let _ = slot_idx; // index reserved for future per-slot diagnostics
        let (host, port, agent_token, is_local) = server_info
            .get(&slot.server_id)
            .cloned()
            .expect("server_info populated above");

        let replicas_arg: Vec<crate::catalog::ReplicaSlot> = replicas_for_server
            .iter()
            .map(|(idx, hp)| {
                let pp = if is_local && public_pending.is_some() {
                    public_pending.take()
                } else {
                    None
                };
                (
                    *idx,
                    vec![(primary_port_name.clone(), *hp, primary_port, pp)],
                )
            })
            .collect();

        let yaml = crate::catalog::build_compose_yaml_scaled(
            &item,
            &id,
            &name,
            &env_vars,
            &replicas_arg,
            network_name.as_deref(),
            !is_local,
        );

        if is_local {
            let _ = crate::docker::compose::down_stack(&stack_name, &state.config).await;
            if let Err(e) =
                crate::docker::deploy_service_stack(&state, &id, &stack_name, &yaml, None).await
            {
                deploy_errors.push(format!("{}: {e}", slot.server_id));
            }
        } else {
            let url = format!(
                "http://{}/api/v1/agent/deploy",
                crate::network::address::authority(&host, port)
            );
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
            let payload = serde_json::json!({
                "stack_name": stack_name,
                "compose_yaml": yaml,
            });
            match client
                .post(&url)
                .header("Authorization", format!("Bearer {agent_token}"))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => deploy_errors.push(format!(
                    "{}: agent responded {}",
                    slot.server_id,
                    resp.status()
                )),
                Err(e) => deploy_errors.push(format!("{}: {e}", slot.server_id)),
            }
        }
    }

    // ── Step 5. Persist service + replica rows ─────────────────────
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE services SET replicas = ?1, lb_strategy = ?2, lb_sticky_cookie = ?3,
                                 updated_at = datetime('now')
             WHERE id = ?4",
            rusqlite::params![total_replicas, strategy_str, sticky_cookie, id],
        )?;
        db.execute("DELETE FROM service_replicas WHERE service_id = ?1", [&id])?;
        for (slot, replicas_for_server) in &per_server_plan {
            for (local_idx, host_port) in replicas_for_server {
                let rid = uuid::Uuid::new_v4().to_string();
                let status = if deploy_errors.iter().any(|e| e.starts_with(&slot.server_id)) {
                    "failed"
                } else {
                    "running"
                };
                db.execute(
                    "INSERT INTO service_replicas
                        (id, service_id, server_id, replica_idx, host_port, weight, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        rid,
                        id,
                        slot.server_id,
                        local_idx,
                        *host_port as i64,
                        slot.weight,
                        status
                    ],
                )?;
            }
        }
    }

    // ── Step 6. Compute upstream list + regenerate Traefik config ──
    let name_slug = slug(&name);
    let mut upstreams: Vec<crate::proxy::config::LbUpstream> = Vec::new();
    for (slot, replicas_for_server) in &per_server_plan {
        let (host, _port, _tok, is_local) = server_info
            .get(&slot.server_id)
            .cloned()
            .expect("server_info populated");
        let single_on_this_server = replicas_for_server.len() == 1;
        for (idx, host_port) in replicas_for_server {
            let url = if is_local {
                if single_on_this_server {
                    format!("http://pier-{name_slug}:{primary_port}")
                } else {
                    format!("http://pier-{name_slug}-{idx}:{primary_port}")
                }
            } else {
                format!("http://{host}:{host_port}")
            };
            upstreams.push(crate::proxy::config::LbUpstream {
                url,
                weight: slot.weight.max(1) as u32,
            });
        }
    }

    let domains: Vec<(String, bool, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt =
            db.prepare("SELECT domain, strip_prefix FROM domains WHERE service_id = ?1")?;
        let list: Vec<(String, bool, bool)> = stmt
            .query_map([&id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)? != 0))
            })?
            .filter_map(|r| r.ok())
            .map(|(d, sp)| (d, true, sp))
            .collect();
        list
    };

    if !domains.is_empty() {
        let lb = crate::proxy::config::LbConfig {
            strategy,
            sticky_cookie: sticky_cookie.clone(),
        };
        if let Err(e) = crate::proxy::config::regenerate_service_config_lb(
            &state.config.data_dir,
            &id,
            &domains,
            &upstreams,
            &lb,
        ) {
            tracing::warn!("Failed to regenerate Traefik config: {e}");
        }
    }

    // Public raw TCP (if any) now lives in each service container's compose
    // `ports:` as a direct Docker binding — the per-replica redeploy above
    // already published the operator-toggled public_port on the first local
    // replica. No Traefik TCP route to regenerate.
    //
    // Caveat: with scaled replicas + raw TCP public, traffic to the public
    // port hits only that one replica (kernel won't let two containers bind
    // the same host port). HA for raw TCP across replicas needs a real LB
    // and is out of scope for this migration.

    // ── Step 7. Log + respond ─────────────────────────────────────
    let output = if deploy_errors.is_empty() {
        format!(
            "LB applied: {} replicas across {} server(s), strategy={}",
            total_replicas,
            per_server_plan.len(),
            strategy_str
        )
    } else {
        format!("LB partial failure: {}", deploy_errors.join("; "))
    };
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        record_deployment_log(
            &db,
            &id,
            "load_balance",
            if deploy_errors.is_empty() {
                "running"
            } else {
                "failed"
            },
            &output,
        );
    }

    if !deploy_errors.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "Scale completed with errors: {}",
            deploy_errors.join("; ")
        )));
    }

    let _ = image; // reserved for future use (image metadata in response)

    Ok(Json(serde_json::json!({
        "ok": true,
        "replicas_total": total_replicas,
        "strategy": strategy_str,
        "sticky_cookie": sticky_cookie,
        "distribution": per_server_plan.iter().map(|(slot, reps)| {
            serde_json::json!({
                "server_id": slot.server_id,
                "replicas": reps.iter().map(|(idx, hp)| serde_json::json!({
                    "idx": idx,
                    "host_port": hp,
                })).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    })))
}

fn slug(name: &str) -> String {
    name.to_lowercase().replace(' ', "-")
}

// ── Deployment Logs ─────────────────────────────────────────────────

/// Record a deployment log entry.
fn record_deployment_log(
    db: &rusqlite::Connection,
    service_id: &str,
    action: &str,
    status: &str,
    output: &str,
) {
    // Map service status to deployment status (running → success)
    let deploy_status = match status {
        "running" => "success",
        other => other,
    };
    let id = uuid::Uuid::new_v4().to_string();
    let _ = db.execute(
        "INSERT INTO deployment_logs (id, service_id, action, status, output, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now'))",
        rusqlite::params![id, service_id, action, deploy_status, output],
    );
}

/// GET /api/v1/resources/{id}/deployment-logs
pub async fn deployment_logs(
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
        "SELECT id, action, status, output, started_at, finished_at
         FROM deployment_logs WHERE service_id = ?1
         ORDER BY started_at DESC LIMIT 50",
    )?;
    let logs: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "action": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "output": row.get::<_, String>(3)?,
                "started_at": row.get::<_, String>(4)?,
                "finished_at": row.get::<_, Option<String>>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(logs))
}

// ── Git Configuration ───────────────────────────────────────────────

/// GET /api/v1/resources/{id}/git — get git config for a service.
pub async fn get_git_config(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let config = db
        .query_row(
            "SELECT git_repo_url, git_branch, git_source_id, build_strategy, git_webhook_secret
             FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "git_repo_url": row.get::<_, Option<String>>(0)?,
                    "git_branch": row.get::<_, Option<String>>(1)?,
                    "git_source_id": row.get::<_, Option<String>>(2)?,
                    "build_strategy": row.get::<_, Option<String>>(3)?,
                    "webhook_secret": row.get::<_, Option<String>>(4)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Service {id} not found")))?;

    Ok(Json(config))
}

/// PUT /api/v1/resources/{id}/git — configure git source for a service.
pub async fn update_git_config(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateGitConfigRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    // Generate webhook secret if not provided
    let webhook_secret = body.webhook_secret.unwrap_or_else(|| {
        hex::encode(uuid::Uuid::new_v4().as_bytes().as_slice())
            + &hex::encode(uuid::Uuid::new_v4().as_bytes().as_slice())
    });

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "UPDATE services SET git_repo_url = ?1, git_branch = ?2, git_source_id = ?3, build_strategy = ?4, git_webhook_secret = ?5, updated_at = datetime('now')
         WHERE id = ?6",
        rusqlite::params![
            body.git_repo_url,
            body.git_branch.unwrap_or_else(|| "main".to_string()),
            body.git_source_id,
            body.build_strategy.unwrap_or_else(|| "dockerfile".to_string()),
            webhook_secret,
            id,
        ],
    )?;

    if rows == 0 {
        return Err(AppError::NotFound(format!("Service {id} not found")));
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "webhook_secret": webhook_secret,
    })))
}

#[derive(Deserialize)]
pub struct UpdateGitConfigRequest {
    pub git_repo_url: String,
    pub git_branch: Option<String>,
    pub git_source_id: Option<String>,
    pub build_strategy: Option<String>,
    pub webhook_secret: Option<String>,
}

/// PUT /api/v1/resources/{id}/port-public — toggle public port via direct
/// Docker port binding on the service container.
///
/// Coolify-style architecture: the operator's "Make publicly available"
/// rebuilds the service's docker-compose.yml with an extra
/// `0.0.0.0:{public_port}:{container_port}` line and runs `docker compose up
/// -d`. Traefik is untouched — it never knew about raw TCP routes after this
/// migration. Trade-off: the service container is recreated (~3 s downtime
/// for that service); the platform stays up.
pub async fn set_port_public(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<SetPortPublicRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let public_port = body.public_port.unwrap_or(0) as u16;

    // Disabling public access: if the service has domains attached, require
    // explicit confirmation (the domains will be deleted as part of the master
    // "make this service private" action).
    if !body.is_public {
        let domains: Vec<String> = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            let mut stmt =
                db.prepare("SELECT domain FROM domains WHERE service_id = ?1 ORDER BY domain")?;
            let rows: Vec<String> = stmt
                .query_map([&id], |row| row.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };
        if !domains.is_empty() {
            if body.cascade_delete_domains != Some(true) {
                return Err(AppError::DomainsRequireConfirmation { domains });
            }
            // Cascade-delete: drop domains + tear down Traefik HTTP routes for this service.
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute("DELETE FROM domains WHERE service_id = ?1", [&id])?;
            }
            // Empty domain list deletes the per-service dynamic config file.
            if let Err(e) = crate::proxy::config::regenerate_service_config(
                &state.config.data_dir,
                &id,
                &[],
                "",
            ) {
                tracing::warn!("Failed to remove Traefik config for {id}: {e}");
            }
            tracing::info!(
                "Cascade-deleted {} domain(s) for {id}: {:?}",
                domains.len(),
                domains
            );
        }
    }

    // Snapshot pre-toggle state so a docker-compose failure can roll back
    // is_public/public_port instead of leaving the DB ahead of reality.
    // Acts on exactly one port_allocations row: the one named by
    // `body.port_id`, or the only port if the service has just one.
    let (port_id, prev_is_public, prev_public_port, host_port, container_port, service_name): (
        String,
        bool,
        Option<u16>,
        u16,
        u16,
        String,
    ) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let svc_name: String = db
            .query_row("SELECT name FROM services WHERE id = ?1", [&id], |row| {
                row.get(0)
            })
            .map_err(|_| AppError::NotFound(format!("Service {id} not found")))?;

        // Resolve the target port row.
        let resolved_port_id: String = match body.port_id.as_deref() {
            Some(pid) => {
                // Verify the port_id belongs to this service.
                let owner: Option<String> = db
                    .query_row(
                        "SELECT service_id FROM port_allocations WHERE id = ?1",
                        [pid],
                        |row| row.get(0),
                    )
                    .ok();
                match owner {
                    Some(svc) if svc == id => pid.to_string(),
                    Some(_) => {
                        return Err(AppError::BadRequest(
                            "port_id does not belong to this service".into(),
                        ));
                    }
                    None => return Err(AppError::NotFound(format!("Port {pid} not found"))),
                }
            }
            None => {
                // No explicit port_id: only valid for single-port services so
                // we don't accidentally flip the wrong port (the previous
                // behavior, which silently fanned the toggle out across every
                // port via `UPDATE ... WHERE service_id = ?`, was the source
                // of the multi-port bug).
                let mut stmt = db.prepare(
                    "SELECT id FROM port_allocations WHERE service_id = ?1 ORDER BY rowid",
                )?;
                let ids: Vec<String> = stmt
                    .query_map([&id], |row| row.get::<_, String>(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                match ids.len() {
                    0 => return Err(AppError::NotFound(format!("No ports for resource {id}"))),
                    1 => ids.into_iter().next().unwrap(),
                    _ => {
                        return Err(AppError::BadRequest(
                            "port_id is required for multi-port services".into(),
                        ));
                    }
                }
            }
        };

        let (hp, cp, prev_pub, prev_pp): (i64, i64, i64, Option<i64>) = db
            .query_row(
                "SELECT host_port, container_port, is_public, public_port \
                 FROM port_allocations WHERE id = ?1",
                [&resolved_port_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|_| AppError::NotFound(format!("Port {resolved_port_id} not found")))?;

        if body.is_public {
            // Default to the container port (what user sees in "Internal Network"),
            // falling back to host_port only if container_port is somehow 0.
            let pp = if public_port > 0 {
                public_port
            } else if cp > 0 {
                cp as u16
            } else {
                hp as u16
            };

            // Reject if any other port (in this or any other service) already
            // claims this public port — duplicate `0.0.0.0:p:*` bindings would
            // either fight for the host port or be silently merged by Docker.
            let conflict: Option<(String, String)> = db
                .query_row(
                    "SELECT service_id, id FROM port_allocations \
                     WHERE is_public = 1 AND public_port = ?1 AND id != ?2 \
                     LIMIT 1",
                    rusqlite::params![pp as i64, resolved_port_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .ok();
            if let Some((other_svc, _)) = conflict {
                let msg = if other_svc == id {
                    format!("Port {pp} is already publicly exposed by another port of this service")
                } else {
                    format!(
                        "Port {pp} is already publicly exposed by another service ({other_svc})"
                    )
                };
                return Err(AppError::Conflict(msg));
            }

            db.execute(
                "UPDATE port_allocations SET is_public = 1, public_port = ?1 WHERE id = ?2",
                rusqlite::params![pp as i64, resolved_port_id],
            )?;
        } else {
            db.execute(
                "UPDATE port_allocations SET is_public = 0, public_port = NULL WHERE id = ?1",
                [&resolved_port_id],
            )?;
        }
        (
            resolved_port_id,
            prev_pub == 1,
            prev_pp.map(|p| p as u16),
            hp as u16,
            cp as u16,
            svc_name,
        )
    };

    let pp = if public_port > 0 {
        public_port
    } else if container_port > 0 {
        container_port
    } else {
        host_port
    };

    // Rebuild compose YAML with new public binding and redeploy via docker
    // compose. Failure (e.g. host port already in use) bubbles up as an HTTP
    // 5xx and the DB flip is rolled back so UI and reality stay consistent.
    match rebuild_and_redeploy_for_port_toggle(&state, &id).await {
        Ok(()) => {
            if body.is_public {
                tracing::info!(
                    "Public Docker port {pp} enabled for {id} → {service_name}:{container_port}"
                );
            } else {
                tracing::info!("Public Docker port disabled for {id}");
            }
            Ok(Json(serde_json::json!({
                "ok": true,
                "is_public": body.is_public,
                "public_port": if body.is_public { Some(pp) } else { None },
            })))
        }
        Err(e) => {
            // Roll back the is_public flag on exactly the port we flipped —
            // the compose stack is now in whatever state docker compose left
            // it, but the DB should not claim a public binding that isn't
            // actually live.
            if let Ok(db) = state.db.lock() {
                let _ = if prev_is_public {
                    db.execute(
                        "UPDATE port_allocations SET is_public = 1, public_port = ?1 \
                         WHERE id = ?2",
                        rusqlite::params![prev_public_port.map(|p| p as i64), port_id],
                    )
                } else {
                    db.execute(
                        "UPDATE port_allocations SET is_public = 0, public_port = NULL \
                         WHERE id = ?1",
                        [&port_id],
                    )
                };
            }
            tracing::error!("Port-public toggle for {id}/{port_id} failed; DB rolled back: {e}");
            Err(AppError::Internal(anyhow::anyhow!(
                "Failed to apply public port: {e}"
            )))
        }
    }
}

/// Rebuild the service's docker-compose.yml from the current DB state (env +
/// ports including the operator-toggled `public_port`) and run
/// `docker compose up -d`. Compose detects the changed `ports:` lines and
/// recreates the container automatically.
///
/// Catalog (template-based) services are rebuilt via `build_compose_yaml`;
/// services with an explicit `compose` template come from the catalog as-is
/// (those templates own their own port lines and the toggle is a no-op for
/// them — the operator should set the public port inside the template); git
/// services use `compose_content` already stored in the DB. We re-issue
/// `docker compose up -d` against the saved YAML so Docker just re-applies it.
pub(crate) async fn rebuild_and_redeploy_for_port_toggle(
    state: &crate::state::AppState,
    service_id: &str,
) -> anyhow::Result<()> {
    use std::collections::HashMap;

    // Snapshot everything we need under one DB lock to avoid TOCTOU between
    // reads.
    struct Snap {
        name: String,
        catalog_id: Option<String>,
        compose_content: Option<String>,
        env_json: Option<String>,
        ports: Vec<crate::catalog::ReplicaPortMapping>,
        // (compose_service, public, container) for each port that is currently
        // toggled public. Used by `inject_public_ports_into_compose` to splice
        // 0.0.0.0:public:container into externally-supplied compose YAMLs.
        public_bindings: Vec<crate::catalog::PublicBinding>,
        network_name: Option<String>,
    }

    let snap = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let (name, catalog_id, compose_content, env_json): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = db.query_row(
            "SELECT name, catalog_id, compose_content, env_json \
             FROM services WHERE id = ?1",
            [service_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

        let mut stmt = db.prepare(
            "SELECT port_name, host_port, container_port, is_public, public_port, compose_service \
             FROM port_allocations WHERE service_id = ?1",
        )?;
        type PortRow = (String, u16, u16, bool, Option<u16>, Option<String>);
        let rows: Vec<PortRow> = stmt
            .query_map([service_id], |row| {
                let is_pub: i64 = row.get(3)?;
                let pp: Option<i64> = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u16,
                    row.get::<_, i64>(2)? as u16,
                    is_pub == 1,
                    pp.map(|p| p as u16),
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let ports: Vec<crate::catalog::ReplicaPortMapping> = rows
            .iter()
            .map(|(name, host, container, is_pub, pp, _)| {
                (
                    name.clone(),
                    *host,
                    *container,
                    if *is_pub { *pp } else { None },
                )
            })
            .collect();

        let public_bindings: Vec<crate::catalog::PublicBinding> = rows
            .iter()
            .filter_map(|(_, _, container, is_pub, pp, compose_service)| {
                if *is_pub {
                    pp.map(|p| (compose_service.clone(), p, *container))
                } else {
                    None
                }
            })
            .collect();

        let network_name: Option<String> = db
            .query_row(
                "SELECT n.name FROM networks n \
                 JOIN services s ON s.network_id = n.id WHERE s.id = ?1",
                [service_id],
                |row| row.get(0),
            )
            .ok();

        Snap {
            name,
            catalog_id,
            compose_content,
            env_json,
            ports,
            public_bindings,
            network_name,
        }
    };

    let env: HashMap<String, String> = {
        let decrypted = crate::crypto::decrypt_env_json(snap.env_json.as_deref());
        serde_json::from_str(&decrypted).unwrap_or_default()
    };

    let yaml = if let Some(catalog_id) = snap.catalog_id.as_deref() {
        let item = state
            .catalog
            .iter()
            .find(|i| i.meta.id == catalog_id)
            .cloned();
        match item {
            Some(item) if item.compose.is_none() => crate::catalog::build_compose_yaml(
                &item,
                service_id,
                &snap.name,
                &env,
                &snap.ports,
                snap.network_name.as_deref(),
            ),
            _ => {
                // Compose-template catalog item OR unknown catalog_id:
                // splice public bindings into the saved YAML so the toggle
                // actually emits `-p 0.0.0.0:public:container`. Previously
                // these branches re-deployed the stored YAML verbatim and
                // the toggle was a silent no-op for these services.
                let base = snap
                    .compose_content
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("No compose_content for {service_id}"))?;
                crate::catalog::inject_public_ports_into_compose(&base, &snap.public_bindings)?
            }
        }
    } else {
        // Git-deployed service: same fix as above — the saved YAML must be
        // patched to include the public bindings before redeploy, otherwise
        // `docker compose up -d` happily re-applies the private-only layout.
        let base = snap
            .compose_content
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No compose_content for {service_id}"))?;
        crate::catalog::inject_public_ports_into_compose(&base, &snap.public_bindings)?
    };

    let stack_name = format!("pier-{}", snap.name.to_lowercase().replace(' ', "-"));

    // Snapshot the existing container's network attachments BEFORE the
    // recreate. We locate it by `pier.service.id` label, not by name —
    // production deployments have container names like `pier-postgresql-srv0`
    // that the catalog generator never produces, so name-based lookup is
    // unreliable. After deploy_service_stack we reattach any network the
    // old container had that the new one is missing.
    let preexisting_networks: std::collections::HashSet<String> = {
        use bollard::query_parameters::ListContainersOptions;
        match state
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await
        {
            Ok(list) => {
                let mut out = std::collections::HashSet::new();
                for c in &list {
                    let matches = c
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("pier.service.id"))
                        .is_some_and(|s| s == service_id);
                    if !matches {
                        continue;
                    }
                    if let Some(id) = c.id.as_deref() {
                        if let Ok(info) = state.docker.inspect_container(id, None).await {
                            if let Some(networks) = info
                                .network_settings
                                .as_ref()
                                .and_then(|ns| ns.networks.as_ref())
                            {
                                for net in networks.keys() {
                                    out.insert(net.clone());
                                }
                            }
                        }
                    }
                }
                out
            }
            Err(e) => {
                tracing::warn!("Port-toggle: failed to snapshot networks for {service_id}: {e}");
                std::collections::HashSet::new()
            }
        }
    };

    // Pre-flight: refuse to deploy if any *new* public port is already held
    // on the host by something that isn't this service's existing container.
    // Without this, docker compose up -d would fail with a low-level
    // "bind: address already in use" deep in Bollard land, and the operator
    // is left guessing whether the problem is Pier, Docker, or a rogue host
    // process. Snapshot the set of ports the current container is already
    // publishing so we don't false-positive on bindings we're just re-stating.
    {
        use bollard::query_parameters::ListContainersOptions;
        let mut already_ours: std::collections::HashSet<u16> = std::collections::HashSet::new();
        if let Ok(list) = state
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await
        {
            for c in &list {
                let matches = c
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("pier.service.id"))
                    .is_some_and(|s| s == service_id);
                if !matches {
                    continue;
                }
                if let Some(cid) = c.id.as_deref() {
                    if let Ok(info) = state.docker.inspect_container(cid, None).await {
                        if let Some(bindings) = info.host_config.and_then(|hc| hc.port_bindings) {
                            for entries in bindings.into_values().flatten() {
                                for entry in entries {
                                    if let Some(hp) = entry.host_port.as_deref() {
                                        if let Ok(p) = hp.parse::<u16>() {
                                            already_ours.insert(p);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for (_, public, _) in &snap.public_bindings {
            if already_ours.contains(public) {
                continue;
            }
            // Probe by trying to bind 0.0.0.0:<public>. Drop immediately —
            // the listener exists only long enough to detect a conflict.
            match std::net::TcpListener::bind(("0.0.0.0", *public)) {
                Ok(l) => drop(l),
                Err(e) => {
                    anyhow::bail!(
                        "Host port {public} is already in use by another process (not this container): {e}. \
                         Free the port (e.g. `sudo ss -tlnp '( sport = :{public} )'`) and toggle again."
                    );
                }
            }
        }
    }

    // Persist the rebuilt YAML so subsequent redeploys keep the new port lines.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE services SET compose_content = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![yaml, service_id],
        );
    }

    crate::docker::deploy_service_stack(state, service_id, &stack_name, &yaml, None).await?;

    // Post-deploy: reattach any network the old container had that the new
    // one is missing. This catches operator-attached networks and per-project
    // networks the catalog generator didn't put back into the YAML.
    if !preexisting_networks.is_empty() {
        use bollard::models::{EndpointSettings, NetworkConnectRequest};
        use bollard::query_parameters::ListContainersOptions;
        if let Ok(list) = state
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false,
                ..Default::default()
            }))
            .await
        {
            for c in &list {
                let matches = c
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("pier.service.id"))
                    .is_some_and(|s| s == service_id);
                if !matches {
                    continue;
                }
                let Some(container_id) = c.id.as_deref() else {
                    continue;
                };
                let current: std::collections::HashSet<String> = state
                    .docker
                    .inspect_container(container_id, None)
                    .await
                    .ok()
                    .and_then(|info| {
                        info.network_settings
                            .and_then(|ns| ns.networks)
                            .map(|nets| nets.keys().cloned().collect())
                    })
                    .unwrap_or_default();
                for net in preexisting_networks.difference(&current) {
                    let req = NetworkConnectRequest {
                        container: container_id.to_string(),
                        endpoint_config: Some(EndpointSettings::default()),
                    };
                    match state.docker.connect_network(net, req).await {
                        Ok(()) => tracing::info!(
                            "Port-toggle: re-attached {service_id} container to network {net}"
                        ),
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.contains("already exists in network")
                                && !msg.contains("already attached to network")
                            {
                                tracing::warn!(
                                    "Port-toggle: failed to re-attach {service_id} to {net}: {msg}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
pub struct SetPortPublicRequest {
    pub is_public: bool,
    pub public_port: Option<i64>,
    /// When `is_public=false` and the service has attached domains, the request
    /// is rejected with 409 unless this is `Some(true)`. If true, all domains
    /// for the service are deleted (and their Traefik routes torn down) before
    /// the public TCP port is disabled.
    #[serde(default)]
    pub cascade_delete_domains: Option<bool>,
    /// Target a specific `port_allocations.id`. Required for services with
    /// more than one port; for single-port services it may be omitted and the
    /// only port is used. Wrong service_id ⇒ 404; multi-port + missing ⇒ 400.
    #[serde(default)]
    pub port_id: Option<String>,
}

/// GET /api/v1/resources/{id}/git-compose — fetch the live docker-compose.yml
/// directly from the configured git repo (HEAD of the branch). The file is
/// returned verbatim — no `strip_compose_ports`, no version munging — so the
/// UI can show users exactly what they wrote.
pub async fn get_git_compose(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let yaml = crate::deploy::fetch_compose_from_git(&state, &id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Fetch compose: {e}")))?;
    Ok(Json(serde_json::json!({ "yaml": yaml })))
}

/// GET /api/v1/resources/{id}/compose-services — enumerate the compose
/// services declared in this resource's docker-compose.yml. Used by the UI
/// to render per-service "Domains for {service}" inputs when a stack hosts
/// multiple containers.
///
/// Reads from the live git file (so the UI reflects what's actually
/// deployable now, not what was deployed last). Falls back to the saved
/// compose_content for non-git resources.
pub async fn get_compose_services(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    // Try git first; if no git is configured, use the stored compose_content.
    let yaml = match crate::deploy::fetch_compose_from_git(&state, &id).await {
        Ok(y) => y,
        Err(_) => {
            let stored: Option<String> = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.query_row(
                    "SELECT compose_content FROM services WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .ok()
                .flatten()
            };
            stored.unwrap_or_default()
        }
    };

    // Resolve `${VAR}` in compose ports using the service's stored env_json,
    // matching deploy-time substitution. Otherwise the UI shows ports as
    // missing for services declared as `${PORT}:3401` etc.
    let env = crate::deploy::load_env_map(&state, &id);
    let services = crate::deploy::parse_compose_services(&yaml, &env);
    let items: Vec<serde_json::Value> = services
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "container_name": if s.container_name.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(s.container_name) },
                "ports": s.ports.iter().map(|(h, c)| serde_json::json!({ "host": h, "container": c })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/resources/{id}/reload-compose — re-read the compose file from
/// git, refresh `port_allocations`, and reconcile Traefik TCP routes. Does
/// NOT rebuild the container — only the port topology + proxy config are
/// updated. Useful when only `ports:` changed in git and the user wants the
/// new mapping reflected without a full redeploy.
pub async fn reload_compose(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let yaml = crate::deploy::reload_compose_ports(&state, &id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Reload compose: {e}")))?;
    Ok(Json(serde_json::json!({ "ok": true, "yaml": yaml })))
}

/// PUT /api/v1/resources/{id}/network — change network assignment.
pub async fn set_network(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<SetNetworkRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    // Resolve network name
    let network_name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let name: String = db
            .query_row(
                "SELECT name FROM networks WHERE id = ?1",
                [&body.network_id],
                |row| row.get(0),
            )
            .map_err(|_| AppError::NotFound(format!("Network {} not found", body.network_id)))?;
        db.execute(
            "UPDATE services SET network_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![body.network_id, id],
        )?;
        name
    };

    // Regenerate compose YAML with new network
    let (name, yaml, _catalog_id) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
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

    if let Some(yaml) = yaml {
        // Replace network references in compose YAML
        let new_yaml = replace_network_in_compose(&yaml, &network_name);

        // Save and redeploy
        {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.execute(
                "UPDATE services SET compose_content = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![new_yaml, id],
            )?;
        }

        let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
        let _ = docker::compose::down_stack(&stack_name, &state.config).await;
        let result = docker::deploy_service_stack(&state, &id, &stack_name, &new_yaml, None).await;

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
        result?;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "network_id": body.network_id,
        "network_name": network_name,
    })))
}

/// Replace network references in compose YAML with a new network name.
fn replace_network_in_compose(yaml: &str, new_network: &str) -> String {
    // Rebuild the networks section at the end of the YAML
    let lines: Vec<&str> = yaml.lines().collect();

    // Find where top-level "networks:" starts and remove everything after it
    let mut cut_at = lines.len();
    let mut in_service_networks = false;
    let mut service_net_start = 0;
    let mut service_net_end = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Top-level networks: section (not indented)
        if trimmed == "networks:" && !line.starts_with(' ') {
            cut_at = i;
            break;
        }
        // Service-level networks: (indented with 4 spaces)
        if trimmed == "networks:" && line.starts_with("    ") && !line.starts_with("      ") {
            in_service_networks = true;
            service_net_start = i;
        } else if in_service_networks {
            if trimmed.starts_with("- ") {
                service_net_end = i + 1;
            } else {
                in_service_networks = false;
            }
        }
    }

    // Rebuild service-level networks
    if service_net_start > 0 {
        let mut new_lines: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if i == service_net_start {
                new_lines.push("    networks:".to_string());
                new_lines.push(format!("      - {new_network}"));
                if new_network != "pier-net" {
                    new_lines.push("      - pier-net".to_string());
                }
            } else if i > service_net_start && i < service_net_end {
                continue; // skip old network lines
            } else if i >= cut_at {
                continue; // skip old top-level networks section
            } else {
                new_lines.push(line.to_string());
            }
        }

        // Add top-level networks section
        new_lines.push("networks:".to_string());
        new_lines.push(format!("  {new_network}:"));
        new_lines.push("    external: true".to_string());
        if new_network != "pier-net" {
            new_lines.push("  pier-net:".to_string());
            new_lines.push("    external: true".to_string());
        }

        return new_lines.join("\n");
    }

    // Fallback: just append networks section
    let mut result: String = lines[..cut_at].join("\n");
    result.push_str(&format!(
        "\nnetworks:\n  {new_network}:\n    external: true\n"
    ));
    if new_network != "pier-net" {
        result.push_str("  pier-net:\n    external: true\n");
    }
    result
}

#[derive(Deserialize)]
pub struct SetNetworkRequest {
    pub network_id: String,
}

#[derive(Deserialize)]
pub struct UpdateSettingsRequest {
    pub auto_deploy: Option<bool>,
    pub force_https: Option<bool>,
}

/// PUT /api/v1/resources/{id}/settings — update advanced settings.
pub async fn update_settings(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSettingsRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    if let Some(auto_deploy) = body.auto_deploy {
        db.execute(
            "UPDATE services SET auto_deploy = ?1 WHERE id = ?2",
            rusqlite::params![auto_deploy, id],
        )?;
    }
    if let Some(force_https) = body.force_https {
        db.execute(
            "UPDATE services SET force_https = ?1 WHERE id = ?2",
            rusqlite::params![force_https, id],
        )?;
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// PUT /api/v1/resources/{id}/rename — rename a service (restarts container).
pub async fn rename(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<RenameRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let new_name = body.name.trim().to_lowercase().replace(' ', "-");

    if new_name.is_empty()
        || !new_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AppError::BadRequest(
            "Name must contain only lowercase letters, numbers, hyphens".into(),
        ));
    }

    let old_name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row("SELECT name FROM services WHERE id = ?1", [&id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    if old_name == new_name {
        return Ok(Json(serde_json::json!({"ok": true, "name": new_name})));
    }

    let old_stack = format!("pier-{}", old_name.to_lowercase().replace(' ', "-"));
    let new_stack = format!("pier-{new_name}");

    tracing::info!(
        "Renaming resource '{old_name}' → '{new_name}' (stack: {old_stack} → {new_stack})"
    );

    // 1. Stop old stack
    let _ = docker::compose::down_stack(&old_stack, &state.config).await;

    // 2. Read compose YAML and update container_name
    let stack_dir = state.config.data_dir.join("stacks").join(&old_stack);
    let compose_file = stack_dir.join("docker-compose.yml");
    let mut yaml = if compose_file.exists() {
        std::fs::read_to_string(&compose_file).unwrap_or_default()
    } else {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT compose_content FROM services WHERE id = ?1",
            [&id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap_or(None)
        .unwrap_or_default()
    };

    if yaml.is_empty() {
        return Err(AppError::BadRequest(
            "No compose content found for this service".into(),
        ));
    }

    // Replace container_name
    yaml = yaml.replace(
        &format!("container_name: {old_stack}"),
        &format!("container_name: {new_stack}"),
    );

    // 3. Move stack directory
    let new_stack_dir = state.config.data_dir.join("stacks").join(&new_stack);
    if stack_dir.exists() {
        if let Err(e) = std::fs::rename(&stack_dir, &new_stack_dir) {
            tracing::warn!("Could not rename stack dir: {e}, will create new");
            let _ = std::fs::create_dir_all(&new_stack_dir);
        }
    } else {
        let _ = std::fs::create_dir_all(&new_stack_dir);
    }

    // 4. Update DB
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE services SET name = ?1, compose_content = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![new_name, yaml, id],
        )?;
    }

    // 5. Deploy with new name
    match docker::deploy_service_stack(&state, &id, &new_stack, &yaml, None).await {
        Ok(out) => tracing::info!("Stack {new_stack} deployed: {out}"),
        Err(e) => {
            tracing::error!("Failed to deploy renamed stack {new_stack}: {e}");
            return Err(AppError::Internal(anyhow::anyhow!(
                "Deploy after rename failed: {e}"
            )));
        }
    }

    Ok(Json(serde_json::json!({"ok": true, "name": new_name})))
}
