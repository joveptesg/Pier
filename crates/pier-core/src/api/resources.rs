use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::catalog;
use crate::catalog::cluster as cluster_gen;
use crate::db::ports;
use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

use super::domains;

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

/// POST /api/v1/resources — create and deploy a resource from catalog.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateResourceRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }

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
        for (_key, field) in &ui.fields {
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
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                    ))
                },
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

    let allocated_ports = with_db(&state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, name, service_type, status, catalog_id, category, env_json, image)
             VALUES (?1, ?2, ?3, 'compose', 'deploying', ?4, ?5, ?6, ?7)",
            rusqlite::params![
                service_id,
                body.project_id,
                name,
                body.catalog_id,
                item.meta.category,
                serde_json::to_string(&vars).unwrap_or_default(),
                item.docker.as_ref().map(|d| catalog::substitute(&d.image, &vars)),
            ],
        )?;
        Ok(ports::allocate_ports(db, &service_id, &port_specs, port_start, port_end)?)
    })?;

    // Add allocated ports to vars
    let port_mappings: Vec<(String, u16, u16)> = allocated_ports
        .iter()
        .map(|pa| {
            vars.insert(
                format!("port_{}", pa.port_name),
                pa.host_port.to_string(),
            );
            (
                pa.port_name.clone(),
                pa.host_port as u16,
                pa.container_port as u16,
            )
        })
        .collect();

    // Update service with first allocated port
    if let Some(first_port) = allocated_ports.first() {
        let hp = first_port.host_port;
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
            AppError::BadRequest(format!("'{}' does not support cluster mode", body.catalog_id))
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
            docker::compose::deploy_stack(&stack_name, &cluster_yaml, &state.config).await;

        let status = if deploy_result.is_ok() {
            "running"
        } else {
            "failed"
        };
        let sid = service_id.clone();
        with_db(&state, |db| {
            let _ = db.execute(
                "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![status, sid],
            );
            Ok(())
        })?;

        deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

        // Auto-generate service domain (skip for databases — no HTTP to proxy)
        if item.meta.category != "database" {
            if let Some(first_port) = allocated_ports.first() {
                try_create_service_domain(&state, &service_id, &name, first_port.host_port).await;
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
        catalog::build_compose_yaml(&item, &service_id, &name, &vars, &port_mappings)
    };

    // Deploy (the only await point)
    let deploy_result = docker::compose::deploy_stack(&stack_name, &yaml, &state.config).await;

    // Update status based on result
    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    with_db(&state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![status, yaml_clone, sid],
        );
        Ok(())
    })?;

    // Propagate deploy error
    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain (skip for databases — no HTTP to proxy)
    if item.meta.category != "database" {
        if let Some(first_port) = allocated_ports.first() {
            try_create_service_domain(&state, &service_id, &name, first_port.host_port).await;
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
    let yaml = body
        .config
        .get("yaml")
        .cloned()
        .unwrap_or_default();
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

    let deploy_result = docker::compose::deploy_stack(stack_name, &yaml, &state.config).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![status, sid],
        );
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
    let dockerfile_content = body
        .config
        .get("dockerfile")
        .cloned()
        .unwrap_or_default();
    if dockerfile_content.trim().is_empty() {
        return Err(AppError::BadRequest("Dockerfile content is required".into()));
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
    let stack_dir = state
        .config
        .data_dir
        .join("stacks")
        .join(stack_name);
    tokio::fs::create_dir_all(&stack_dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Create stack dir: {e}")))?;
    tokio::fs::write(stack_dir.join("Dockerfile"), &dockerfile_content)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Write Dockerfile: {e}")))?;

    // Deploy
    let deploy_result = docker::compose::deploy_stack(stack_name, &yaml, &state.config).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![status, yaml_clone, sid],
        );
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain
    try_create_service_domain(state, &service_id, name, host_port as i64).await;

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
    let git_url = body
        .config
        .get("git_url")
        .cloned()
        .unwrap_or_default();
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
    let stack_dir = state
        .config
        .data_dir
        .join("stacks")
        .join(stack_name);
    tokio::fs::create_dir_all(&stack_dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Create stack dir: {e}")))?;

    let repo_dir = stack_dir.join("repo");

    // If using deploy key, write it and configure SSH
    if use_deploy_key {
        let deploy_key = body
            .config
            .get("deploy_key")
            .cloned()
            .unwrap_or_default();
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
    let deploy_result = docker::compose::deploy_stack(stack_name, &yaml, &state.config).await;

    let status = if deploy_result.is_ok() {
        "running"
    } else {
        "failed"
    };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    let env_data = serde_json::json!({
        "GIT_URL": git_url,
        "GIT_BRANCH": branch,
        "DOCKERFILE_PATH": build_path,
    });
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, env_json = ?3, updated_at = datetime('now') WHERE id = ?4",
            rusqlite::params![status, yaml_clone, env_data.to_string(), sid],
        );
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain
    if let Some(first_port) = allocated_ports.first() {
        try_create_service_domain(state, &service_id, name, first_port.host_port).await;
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
    stack_name: &str,
    item: &catalog::CatalogItem,
) -> AppResult<Json<serde_json::Value>> {
    let source_id = body
        .source_id
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("source_id is required for GitHub App deploy".into()))?;

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
        let app_id = row.0.ok_or_else(|| AppError::BadRequest("Source missing app_id".into()))?;
        let inst_id = row.1.ok_or_else(|| AppError::BadRequest("Source missing installation_id".into()))?;
        let pk = row.2.ok_or_else(|| AppError::BadRequest("Source missing private_key".into()))?;
        Ok((app_id, inst_id, pk))
    })?;

    // Get installation access token
    let token = crate::git::github_app::get_installation_token(&app_id, installation_id, &private_key)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("GitHub App token: {e}")))?;

    let git_url = body
        .config
        .get("git_url")
        .cloned()
        .unwrap_or_default();
    if git_url.trim().is_empty() {
        return Err(AppError::BadRequest("Repository URL is required".into()));
    }

    // Inject token into clone URL: https://x-access-token:{token}@github.com/owner/repo.git
    let clone_url = if git_url.starts_with("https://") {
        git_url.replacen("https://", &format!("https://x-access-token:{token}@"), 1)
    } else {
        return Err(AppError::BadRequest("GitHub App requires HTTPS URL".into()));
    };

    let branch = body.config.get("branch").cloned().filter(|b| !b.is_empty()).unwrap_or_else(|| "main".to_string());
    let build_path = body.config.get("build_path").cloned().filter(|b| !b.is_empty()).unwrap_or_else(|| "/Dockerfile".to_string());
    let container_port: u16 = body.config.get("port").and_then(|p| p.parse().ok()).unwrap_or(3000);

    let service_id = uuid::Uuid::new_v4().to_string();

    let allocated_ports = with_db(state, |db| {
        db.execute(
            "INSERT INTO services (id, project_id, name, service_type, status, catalog_id, category, image)
             VALUES (?1, ?2, ?3, 'compose', 'deploying', ?4, ?5, ?6)",
            rusqlite::params![service_id, body.project_id, name, body.catalog_id, item.meta.category, format!("git: {}", git_url)],
        )?;
        let port_specs = vec![("primary".to_string(), container_port)];
        Ok(ports::allocate_ports(db, &service_id, &port_specs, state.config.port_range_start, state.config.port_range_end)?)
    })?;

    let host_port = allocated_ports.first().map(|p| p.host_port as u16).unwrap_or(container_port);

    let sid = service_id.clone();
    with_db(state, |db| {
        let _ = db.execute("UPDATE services SET port = ?1 WHERE id = ?2", rusqlite::params![host_port as i64, sid]);
        Ok(())
    })?;

    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    tokio::fs::create_dir_all(&stack_dir).await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Create stack dir: {e}")))?;

    let repo_dir = stack_dir.join("repo");

    // Clone with token-injected URL
    let clone_output = tokio::process::Command::new("git")
        .args(["clone", "--depth", "1", "--branch", &branch, &clone_url])
        .arg(repo_dir.to_string_lossy().as_ref())
        .current_dir(&stack_dir)
        .output()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Git clone: {e}")))?;

    if !clone_output.status.success() {
        let stderr = String::from_utf8_lossy(&clone_output.stderr);
        let sid = service_id.clone();
        with_db(state, |db| {
            let _ = db.execute("UPDATE services SET status = 'failed', updated_at = datetime('now') WHERE id = ?1", rusqlite::params![sid]);
            Ok(())
        })?;
        return Err(AppError::Internal(anyhow::anyhow!("Git clone failed: {stderr}")));
    }

    // Verify Dockerfile
    let dockerfile_rel = build_path.trim_start_matches('/');
    let dockerfile_in_repo = if dockerfile_rel.is_empty() { "Dockerfile".to_string() } else { dockerfile_rel.to_string() };
    let full_dockerfile_path = repo_dir.join(&dockerfile_in_repo);
    if !full_dockerfile_path.exists() {
        let sid = service_id.clone();
        with_db(state, |db| {
            let _ = db.execute("UPDATE services SET status = 'failed', updated_at = datetime('now') WHERE id = ?1", rusqlite::params![sid]);
            Ok(())
        })?;
        return Err(AppError::BadRequest(format!("Dockerfile not found at {}", dockerfile_in_repo)));
    }

    // Build compose YAML
    let (context_path, dockerfile_path) = if dockerfile_in_repo.contains('/') {
        let parts: Vec<&str> = dockerfile_in_repo.rsplitn(2, '/').collect();
        (format!("./repo/{}", parts[1]), parts[0].to_string())
    } else {
        ("./repo".to_string(), dockerfile_in_repo.clone())
    };

    let yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   build:\n\
         \x20     context: {context_path}\n\
         \x20     dockerfile: {dockerfile_path}\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"{host_port}:{container_port}\"\n\
         \x20   restart: unless-stopped\n\
         \x20   labels:\n\
         \x20     pier.service.id: \"{service_id}\"\n\
         \x20     pier.catalog.id: \"git-github-app\"\n"
    );

    let deploy_result = docker::compose::deploy_stack(stack_name, &yaml, &state.config).await;

    let status = if deploy_result.is_ok() { "running" } else { "failed" };
    let yaml_clone = yaml.clone();
    let sid = service_id.clone();
    let env_data = serde_json::json!({
        "GIT_URL": git_url,
        "GIT_BRANCH": branch,
        "DOCKERFILE_PATH": build_path,
        "SOURCE_ID": source_id,
    });
    with_db(state, |db| {
        let _ = db.execute(
            "UPDATE services SET status = ?1, compose_content = ?2, env_json = ?3, updated_at = datetime('now') WHERE id = ?4",
            rusqlite::params![status, yaml_clone, env_data.to_string(), sid],
        );
        Ok(())
    })?;

    deploy_result.map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy failed: {e}")))?;

    // Auto-generate service domain
    if let Some(first_port) = allocated_ports.first() {
        try_create_service_domain(state, &service_id, name, first_port.host_port).await;
    }

    let ports_json: Vec<serde_json::Value> = allocated_ports
        .iter()
        .map(|pa| serde_json::json!({"name": pa.port_name, "host": pa.host_port, "container": pa.container_port}))
        .collect();

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": service_id,
        "name": name,
        "status": "running",
        "ports": ports_json,
    })))
}

/// GET /api/v1/resources — list all deployed resources.
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, project_id, name, service_type, status, port, image, catalog_id, category, created_at
         FROM services WHERE catalog_id IS NOT NULL ORDER BY created_at DESC",
    )?;

    let resources: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
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
                "created_at": row.get::<_, String>(9)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(resources))
}

/// GET /api/v1/resources/{id} — get resource details with ports.
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let resource = db
        .query_row(
            "SELECT id, project_id, name, service_type, status, port, image, catalog_id, category, env_json, compose_content, created_at, cluster_mode, cluster_config_json
             FROM services WHERE id = ?1",
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
                    "env_json": row.get::<_, Option<String>>(9)?,
                    "compose_content": row.get::<_, Option<String>>(10)?,
                    "created_at": row.get::<_, String>(11)?,
                    "cluster_mode": row.get::<_, Option<String>>(12)?,
                    "cluster_config_json": row.get::<_, Option<String>>(13)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?;

    let port_allocs = ports::get_ports(&db, &id)?;
    let ports_json: Vec<serde_json::Value> = port_allocs
        .iter()
        .map(|pa| {
            serde_json::json!({
                "name": pa.port_name,
                "host": pa.host_port,
                "container": pa.container_port,
                "protocol": pa.protocol,
            })
        })
        .collect();

    let mut result = resource;
    result["ports"] = serde_json::json!(ports_json);
    Ok(Json(result))
}

/// DELETE /api/v1/resources/{id} — stop and remove a resource.
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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

    let _ = docker::compose::down_stack(&stack_name, &state.config).await;
    let _ = docker::compose::remove_stack(&stack_name, &state.config).await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    ports::free_ports(&db, &id)?;
    db.execute("DELETE FROM services WHERE id = ?1", [&id])?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/resources/{id}/stop
pub async fn stop(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
    record_deployment_log(&db, &id, "stop", status_str, "");
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
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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

    let result = docker::compose::deploy_stack(&stack_name, &yaml, &state.config).await;

    let status = if result.is_ok() { "running" } else { "failed" };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    record_deployment_log(&db, &id, "start", if result.is_ok() { "success" } else { "failed" }, "");
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
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
    let result = docker::compose::deploy_stack(&stack_name, &yaml, &state.config).await;

    let status = if result.is_ok() { "running" } else { "failed" };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    record_deployment_log(&db, &id, "restart", if result.is_ok() { "success" } else { "failed" }, "");
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
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
    Path(id): Path<String>,
    Json(body): Json<ScaleRequest>,
) -> AppResult<impl IntoResponse> {
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

    let catalog_id = catalog_id
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("No catalog_id")))?;

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

    // Parse existing vars
    let vars: HashMap<String, String> = env_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

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
            body.server_id.clone().unwrap_or_else(|| "localhost".to_string())
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
    let deploy_result = docker::compose::deploy_stack(&stack_name, &new_yaml, &state.config).await;

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

// ── Deployment Logs ─────────────────────────────────────────────────

/// Record a deployment log entry.
fn record_deployment_log(
    db: &rusqlite::Connection,
    service_id: &str,
    action: &str,
    status: &str,
    output: &str,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let _ = db.execute(
        "INSERT INTO deployment_logs (id, service_id, action, status, output, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now'))",
        rusqlite::params![id, service_id, action, status, output],
    );
}

/// GET /api/v1/resources/{id}/deployment-logs
pub async fn deployment_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
