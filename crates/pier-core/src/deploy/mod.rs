pub mod build;
pub mod rollback;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::docker;
use crate::state::AppState;

/// Information about the commit that triggered the deploy.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
    pub branch: String,
}

/// Run the full deploy pipeline for a service.
///
/// 1. Create a `deployments` record (pending)
/// 2. Clone the repo
/// 3. Build (Dockerfile or docker-compose)
/// 4. Swap image and redeploy containers
/// 5. Record result
pub async fn run_pipeline(
    state: Arc<AppState>,
    service_id: String,
    commit: CommitInfo,
    triggered_by: &str,
) {
    let start = Instant::now();

    // Read service config from DB
    let svc = {
        let db = match state.db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!("Pipeline DB lock failed: {e}");
                return;
            }
        };
        db.query_row(
            "SELECT name, git_repo_url, git_branch, git_source_id, build_strategy, compose_content, image
             FROM services WHERE id = ?1",
            [&service_id],
            |row| {
                Ok(ServiceInfo {
                    name: row.get(0)?,
                    git_repo_url: row.get(1)?,
                    git_branch: row.get(2)?,
                    git_source_id: row.get(3)?,
                    build_strategy: row.get(4)?,
                    compose_content: row.get(5)?,
                    current_image: row.get(6)?,
                })
            },
        )
        .ok()
    };

    let svc = match svc {
        Some(s) => s,
        None => {
            tracing::error!("Pipeline: service {service_id} not found");
            return;
        }
    };

    let deploy_id = uuid::Uuid::new_v4().to_string();
    let sha_short = if commit.sha.len() >= 12 {
        &commit.sha[..12]
    } else {
        &commit.sha
    };
    let image_tag = format!(
        "pier-{}:{}",
        svc.name.to_lowercase().replace(' ', "-"),
        sha_short
    );

    // Create deployment record
    {
        let db = match state.db.lock() {
            Ok(db) => db,
            Err(_) => return,
        };
        let _ = db.execute(
            "INSERT INTO deployments (id, service_id, commit_sha, commit_message, branch, status, triggered_by, image_tag)
             VALUES (?1, ?2, ?3, ?4, ?5, 'building', ?6, ?7)",
            rusqlite::params![
                deploy_id,
                service_id,
                commit.sha,
                commit.message,
                commit.branch,
                triggered_by,
                image_tag,
            ],
        );
        let _ = db.execute(
            "UPDATE services SET status = 'deploying', updated_at = datetime('now') WHERE id = ?1",
            [&service_id],
        );
    }

    let stack_name = format!("pier-{}", svc.name.to_lowercase().replace(' ', "-"));
    let strategy = svc.build_strategy.as_deref().unwrap_or("dockerfile");

    // Write .env file from service env_json
    write_env_file(&state, &service_id, &stack_name).await;

    // Get clone URL (may need GitHub App token injection)
    let clone_url = match resolve_clone_url(&state, &svc).await {
        Ok(url) => url,
        Err(e) => {
            finish_deployment(
                &state,
                &deploy_id,
                &service_id,
                "failed",
                &format!("Clone URL resolve: {e}"),
                start,
            );
            return;
        }
    };

    let branch = svc.git_branch.as_deref().unwrap_or("main");

    // Helper: flush current log to DB so frontend can show progress
    let flush_log = |state: &AppState, deploy_id: &str, log: &str| {
        if let Ok(db) = state.db.lock() {
            let _ = db.execute(
                "UPDATE deployments SET build_log = ?1 WHERE id = ?2",
                rusqlite::params![log, deploy_id],
            );
        }
    };

    // Clone
    let repo_dir = state.config.data_dir.join("tmp").join(&deploy_id);
    let mut log = String::new();

    log.push_str("Cloning repository...\n");
    flush_log(&state, &deploy_id, &log);

    match build::clone_repo(&clone_url, branch, &repo_dir).await {
        Ok(output) => { log.push_str(&output); flush_log(&state, &deploy_id, &log); }
        Err(e) => {
            log.push_str(&format!("Clone failed: {e}\n"));
            finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
            let _ = tokio::fs::remove_dir_all(&repo_dir).await;
            return;
        }
    }

    // Build
    log.push_str("Building...\n");
    flush_log(&state, &deploy_id, &log);

    match strategy {
        "dockerfile" => {
            match build::docker_build(&state.docker, &repo_dir, &image_tag).await {
                Ok(output) => { log.push_str(&output); flush_log(&state, &deploy_id, &log); }
                Err(e) => {
                    log.push_str(&format!("Build failed: {e}\n"));
                    finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                    let _ = tokio::fs::remove_dir_all(&repo_dir).await;
                    return;
                }
            }

            // Update compose with new image tag and redeploy
            let yaml = build::generate_compose_for_image(
                &svc.name,
                &stack_name,
                &image_tag,
                &state,
                &service_id,
            );

            log.push_str("Deploying...\n");
            flush_log(&state, &deploy_id, &log);

            match docker::compose::deploy_stack(&stack_name, &yaml, &state.config).await {
                Ok(output) => {
                    log.push_str(&format!("Deploy: {output}\n"));
                    flush_log(&state, &deploy_id, &log);
                }
                Err(e) => {
                    log.push_str(&format!("Deploy failed: {e}\n"));
                    finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                    let _ = tokio::fs::remove_dir_all(&repo_dir).await;
                    return;
                }
            }
        }
        "docker-compose" => {
            // Use repo's own docker-compose.yml — run from repo dir (needed for build: context)
            let compose_file = repo_dir.join("docker-compose.yml");
            if !compose_file.exists() {
                log.push_str("docker-compose.yml not found in repo\n");
                finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                let _ = tokio::fs::remove_dir_all(&repo_dir).await;
                return;
            }

            // Write .env from service env_json into repo dir
            write_env_file(&state, &service_id, &stack_name).await;
            // Also copy .env to repo dir for docker-compose build context
            let stack_env = state.config.data_dir.join("stacks").join(&stack_name).join(".env");
            if stack_env.exists() {
                let _ = tokio::fs::copy(&stack_env, repo_dir.join(".env")).await;
            }

            match tokio::fs::read_to_string(&compose_file).await {
                Ok(yaml) => {
                    let yaml = strip_compose_version(&yaml);
                    // Extract ports before stripping (for port_allocations DB update)
                    extract_and_save_ports(&state, &service_id, &yaml);
                    // Inject pier-net (and project network) so services can communicate across stacks
                    let yaml = inject_pier_networks(&state, &service_id, &yaml);
                    // Remove host port bindings (Traefik handles public access via Docker network)
                    let yaml = strip_compose_ports(&yaml);

                    // Move repo contents to stack dir so build context works from persistent location
                    let stack_dir = state.config.data_dir.join("stacks").join(&stack_name);
                    let _ = tokio::fs::remove_dir_all(&stack_dir).await;
                    if let Err(e) = tokio::fs::rename(&repo_dir, &stack_dir).await {
                        // rename may fail across filesystems, fall back to copy
                        log.push_str(&format!("Move repo to stack dir: {e}, trying copy\n"));
                        let _ = tokio::fs::create_dir_all(&stack_dir).await;
                        let _ = copy_dir_all(&repo_dir, &stack_dir).await;
                    }

                    // Write cleaned compose YAML
                    let _ = tokio::fs::write(stack_dir.join("docker-compose.yml"), &yaml).await;

                    // Build and deploy from stack dir (contains source code + Dockerfile)
                    log.push_str("Building & deploying with docker-compose...\n");
                    flush_log(&state, &deploy_id, &log);

                    let output = tokio::process::Command::new("docker")
                        .args(["compose", "-p", &stack_name, "up", "-d", "--build"])
                        .current_dir(&stack_dir)
                        .env("HOME", state.config.data_dir.parent().unwrap_or(&state.config.data_dir))
                        .output()
                        .await;

                    match output {
                        Ok(out) => {
                            let combined = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                            if out.status.success() {
                                log.push_str(&format!("Deploy: {combined}\n"));
                                flush_log(&state, &deploy_id, &log);
                            } else {
                                log.push_str(&format!("Deploy failed: {combined}\n"));
                                finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                                return;
                            }
                        }
                        Err(e) => {
                            log.push_str(&format!("Deploy failed: {e}\n"));
                            finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                            return;
                        }
                    }
                }
                Err(e) => {
                    log.push_str(&format!("Read compose file: {e}\n"));
                    finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
                    let _ = tokio::fs::remove_dir_all(&repo_dir).await;
                    return;
                }
            }
        }
        other => {
            log.push_str(&format!("Unknown build strategy: {other}\n"));
            finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
            let _ = tokio::fs::remove_dir_all(&repo_dir).await;
            return;
        }
    }

    // Detect actual container name(s) after deploy
    let actual_container_name = detect_container_name(&stack_name, &state.config).await;

    // Read compose content from stack dir
    let stack_dir = state.config.data_dir.join("stacks").join(&stack_name);
    let compose_path = stack_dir.join("docker-compose.yml");
    let compose_content = std::fs::read_to_string(&compose_path).unwrap_or_default();

    // Save previous image tag, compose content, and container name
    {
        if let Ok(db) = state.db.lock() {
            let _ = db.execute(
                "UPDATE services SET previous_image_tag = image, image = ?1, compose_content = ?3, container_id = ?4, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![image_tag, service_id, compose_content, actual_container_name],
            );
        }
    }

    // Update ports from compose (works for dockerfile strategy; docker-compose ports already extracted before strip)
    update_ports_from_compose(&state, &service_id, &compose_content);

    // Persist env vars from .env file to env_json (for canvas dependency detection)
    persist_env_from_disk(&state, &service_id, &stack_name);

    finish_deployment(&state, &deploy_id, &service_id, "success", &log, start);

    // Cleanup temp dir
    let _ = tokio::fs::remove_dir_all(&repo_dir).await;

    tracing::info!("Pipeline complete for {service_id}: deploy {deploy_id} succeeded");
}

/// Resolve the clone URL, injecting auth tokens if needed.
async fn resolve_clone_url(state: &AppState, svc: &ServiceInfo) -> Result<String> {
    let repo_url = svc
        .git_repo_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("No git_repo_url configured"))?;

    let source_id = match svc.git_source_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return Ok(repo_url.to_string()),
    };

    // Check source type
    let source_type: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT source_type FROM git_sources WHERE id = ?1",
            [source_id],
            |row| row.get(0),
        )?
    };

    match source_type.as_str() {
        "github-app" => {
            let (app_id, installation_id, private_key) = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.query_row(
                    "SELECT app_id, installation_id, private_key FROM git_sources WHERE id = ?1",
                    [source_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?
            };

            let app_id = app_id.ok_or_else(|| anyhow::anyhow!("Missing app_id"))?;
            let inst_id =
                installation_id.ok_or_else(|| anyhow::anyhow!("Missing installation_id"))?;
            let pk = private_key.ok_or_else(|| anyhow::anyhow!("Missing private_key"))?;

            let token =
                crate::git::github_app::get_installation_token(&app_id, inst_id, &pk).await?;

            if repo_url.starts_with("https://") {
                Ok(repo_url.replacen("https://", &format!("https://x-access-token:{token}@"), 1))
            } else {
                Ok(repo_url.to_string())
            }
        }
        "github" | "gitlab" => {
            // Token-based auth
            let token: Option<String> = {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.query_row(
                    "SELECT access_token FROM git_sources WHERE id = ?1",
                    [source_id],
                    |row| row.get(0),
                )?
            };

            if let Some(token) = token.filter(|t| !t.is_empty()) {
                if repo_url.starts_with("https://") {
                    Ok(repo_url.replacen("https://", &format!("https://oauth2:{token}@"), 1))
                } else {
                    Ok(repo_url.to_string())
                }
            } else {
                Ok(repo_url.to_string())
            }
        }
        _ => Ok(repo_url.to_string()),
    }
}

/// Update deployment record and service status on completion.
fn finish_deployment(
    state: &AppState,
    deploy_id: &str,
    service_id: &str,
    status: &str,
    log: &str,
    start: Instant,
) {
    let duration = start.elapsed().as_secs() as i64;
    let service_status = if status == "success" {
        "running"
    } else {
        "failed"
    };

    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "UPDATE deployments SET status = ?1, build_log = ?2, duration_secs = ?3, finished_at = datetime('now') WHERE id = ?4",
            rusqlite::params![status, log, duration, deploy_id],
        );
        let _ = db.execute(
            "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![service_status, service_id],
        );
    }
}

/// Inject pier-net (and project network) into a docker-compose YAML from a repo.
/// This ensures services can communicate with other Pier services via Docker DNS.
fn inject_pier_networks(state: &AppState, service_id: &str, yaml: &str) -> String {
    // Get the service's assigned network name
    let network_name: String = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT n.name FROM networks n JOIN services s ON s.network_id = n.id WHERE s.id = ?1",
                [service_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "pier-net".to_string());

    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();

    // Find all service names (lines under "services:" with proper indentation)
    let mut service_indices = Vec::new();
    let mut in_services = false;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "services:" {
            in_services = true;
            continue;
        }
        if in_services && !trimmed.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
            in_services = false; // new top-level key
        }
        if in_services && !trimmed.is_empty() && trimmed.ends_with(':') && (line.starts_with("  ") || line.starts_with('\t')) && !line.starts_with("    ") {
            service_indices.push(i);
        }
    }

    // For each service: remove existing networks section and add pier networks
    let net_replacement = if network_name == "pier-net" {
        "    networks:\n      - pier-net".to_string()
    } else {
        format!("    networks:\n      - {network_name}\n      - pier-net")
    };

    for &idx in service_indices.iter().rev() {
        let mut end = lines.len();
        for j in (idx + 1)..lines.len() {
            let line = &lines[j];
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if (line.starts_with("  ") || line.starts_with('\t')) && !line.starts_with("    ") && !line.starts_with("\t\t") && trimmed.ends_with(':') {
                end = j;
                break;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                end = j;
                break;
            }
        }

        // Remove existing service-level networks section
        let mut net_start = None;
        let mut net_end = None;
        for j in idx..end {
            let trimmed = lines[j].trim();
            if trimmed == "networks:" && (lines[j].starts_with("    ") || lines[j].starts_with("\t\t")) {
                net_start = Some(j);
            } else if net_start.is_some() && net_end.is_none() {
                if !trimmed.starts_with("- ") && !trimmed.is_empty() {
                    net_end = Some(j);
                }
            }
        }
        if let Some(start) = net_start {
            let end_idx = net_end.unwrap_or(end);
            for _ in start..end_idx {
                if start < lines.len() { lines.remove(start); }
            }
        }

        // Find new end after removal
        let mut new_end = lines.len();
        for j in (idx + 1)..lines.len() {
            let line = &lines[j];
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if (line.starts_with("  ") || line.starts_with('\t')) && !line.starts_with("    ") && !line.starts_with("\t\t") && trimmed.ends_with(':') {
                new_end = j;
                break;
            }
            if !line.starts_with(' ') && !line.starts_with('\t') {
                new_end = j;
                break;
            }
        }

        // Insert pier networks
        lines.insert(new_end, net_replacement.clone());
    }

    // Remove existing top-level networks section
    let mut networks_start = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "networks:" && !line.starts_with(' ') {
            networks_start = Some(i);
            break;
        }
    }
    if let Some(start) = networks_start {
        let mut end = lines.len();
        for j in (start + 1)..lines.len() {
            if !lines[j].starts_with(' ') && !lines[j].starts_with('\t') && !lines[j].trim().is_empty() {
                end = j;
                break;
            }
        }
        lines.drain(start..end);
    }

    // Append networks section
    lines.push("networks:".to_string());
    lines.push(format!("  {network_name}:"));
    lines.push("    external: true".to_string());
    if network_name != "pier-net" {
        lines.push("  pier-net:".to_string());
        lines.push("    external: true".to_string());
    }

    lines.join("\n")
}

/// Parse ports from compose YAML and update port_allocations in DB.
/// Handles formats: "5201:5201", "127.0.0.1:5201:5201", "3000:3000/tcp"
fn extract_and_save_ports(state: &AppState, service_id: &str, yaml: &str) {
    update_ports_from_compose(state, service_id, yaml);
}

fn update_ports_from_compose(state: &AppState, service_id: &str, yaml: &str) {
    let mut ports = Vec::new();
    let mut in_ports = false;

    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed == "ports:" {
            in_ports = true;
            continue;
        }
        if in_ports {
            if trimmed.starts_with("- ") {
                let port_str = trimmed.strip_prefix("- ").unwrap_or("").trim().trim_matches('"').trim_matches('\'');
                // Remove protocol suffix (/tcp, /udp)
                let port_str = port_str.split('/').next().unwrap_or(port_str);
                // Parse: "host:container" or "ip:host:container"
                let parts: Vec<&str> = port_str.split(':').collect();
                match parts.len() {
                    2 => {
                        if let (Ok(host), Ok(container)) = (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
                            ports.push((host, container));
                        }
                    }
                    3 => {
                        if let (Ok(host), Ok(container)) = (parts[1].parse::<u16>(), parts[2].parse::<u16>()) {
                            ports.push((host, container));
                        }
                    }
                    1 => {
                        if let Ok(p) = parts[0].parse::<u16>() {
                            ports.push((p, p));
                        }
                    }
                    _ => {}
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                in_ports = false;
            }
        }
    }

    if ports.is_empty() { return; }

    if let Ok(db) = state.db.lock() {
        // Delete old port allocations for this service
        let _ = db.execute("DELETE FROM port_allocations WHERE service_id = ?1", [service_id]);

        // Insert new ports from compose
        for (i, (host_port, container_port)) in ports.iter().enumerate() {
            let port_name = if i == 0 { "primary".to_string() } else { format!("port-{}", i) };
            let id = uuid::Uuid::new_v4().to_string();
            let _ = db.execute(
                "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol) VALUES (?1, ?2, ?3, ?4, ?5, 'tcp')",
                rusqlite::params![id, service_id, port_name, *host_port as i64, *container_port as i64],
            );
        }

        // Update service port field with first port
        if let Some((hp, _)) = ports.first() {
            let _ = db.execute(
                "UPDATE services SET port = ?1 WHERE id = ?2",
                rusqlite::params![*hp as i64, service_id],
            );
        }

        tracing::info!("Updated ports from compose for {service_id}: {:?}", ports);
    }
}

/// Detect the actual container name after docker compose deploy.
/// Uses `docker compose -p {project} ps --format {{.Name}}` to find running containers.
async fn detect_container_name(stack_name: &str, config: &crate::config::PierConfig) -> String {
    let stack_dir = config.data_dir.join("stacks").join(stack_name);
    let output = tokio::process::Command::new("docker")
        .args(["compose", "-p", stack_name, "ps", "--format", "{{.Name}}"])
        .current_dir(&stack_dir)
        .env("HOME", config.data_dir.parent().unwrap_or(&config.data_dir))
        .output()
        .await;

    match output {
        Ok(out) => {
            let names = String::from_utf8_lossy(&out.stdout);
            let first = names.lines().next().unwrap_or("").trim().to_string();
            if !first.is_empty() {
                tracing::info!("Detected container name: {first} for stack {stack_name}");
                first
            } else {
                stack_name.to_string()
            }
        }
        Err(_) => stack_name.to_string(),
    }
}

/// Recursively copy a directory.
async fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dst).await?;
    let mut entries = tokio::fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().await?.is_dir() {
            Box::pin(copy_dir_all(&src_path, &dst_path)).await?;
        } else {
            tokio::fs::copy(&src_path, &dst_path).await?;
        }
    }
    Ok(())
}

/// Strip `ports:` sections from service blocks in docker-compose YAML.
/// Ports are extracted separately and managed by Pier (no host port bindings needed).
fn strip_compose_ports(yaml: &str) -> String {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result = Vec::new();
    let mut skip_ports = false;

    for line in &lines {
        let trimmed = line.trim();
        // Detect service-level ports: (indented with 4+ spaces)
        if trimmed == "ports:" && (line.starts_with("    ") || line.starts_with("\t\t")) {
            skip_ports = true;
            continue;
        }
        if skip_ports {
            if trimmed.starts_with("- ") {
                continue; // skip port entries
            }
            skip_ports = false;
        }
        result.push(*line);
    }

    result.join("\n")
}

/// Strip the obsolete `version:` field from docker-compose YAML.
fn strip_compose_version(yaml: &str) -> String {
    yaml.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("version:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write an .env file to the stack directory from the service's env_json.
async fn write_env_file(state: &AppState, service_id: &str, stack_name: &str) {
    let env_json: Option<String> = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT env_json FROM services WHERE id = ?1",
                [service_id],
                |row| row.get(0),
            )
            .ok()
        })
        .flatten();

    let env_content = match env_json {
        Some(json_str) => {
            match serde_json::from_str::<serde_json::Value>(&json_str) {
                Ok(serde_json::Value::Object(map)) => {
                    let mut lines = Vec::new();
                    for (k, v) in &map {
                        let val = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        lines.push(format!("{k}={val}"));
                    }
                    lines.join("\n")
                }
                _ => String::new(),
            }
        }
        None => String::new(),
    };

    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    let env_path = stack_dir.join(".env");
    let _ = tokio::fs::create_dir_all(&stack_dir).await;
    if let Err(e) = tokio::fs::write(&env_path, &env_content).await {
        tracing::warn!("Failed to write .env for {stack_name}: {e}");
    }
}

/// Read .env from stack dir and save to services.env_json if currently empty.
/// This ensures canvas dependency detection works for git-deployed services.
fn persist_env_from_disk(state: &AppState, service_id: &str, stack_name: &str) {
    let db = match state.db.lock() {
        Ok(db) => db,
        Err(_) => return,
    };

    // Check if env_json already has data
    let current: Option<String> = db
        .query_row(
            "SELECT env_json FROM services WHERE id = ?1",
            [service_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    if let Some(ref val) = current {
        if !val.is_empty() && val != "{}" && val != "null" {
            return; // Already has env data, don't overwrite
        }
    }

    // Read .env from stack dir
    let env_path = state
        .config
        .data_dir
        .join("stacks")
        .join(stack_name)
        .join(".env");
    let content = match std::fs::read_to_string(&env_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut env_map = serde_json::Map::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let val = val.trim_matches('"').trim_matches('\'');
            env_map.insert(
                key.trim().to_string(),
                serde_json::Value::String(val.to_string()),
            );
        }
    }

    if env_map.is_empty() {
        return;
    }

    if let Ok(json_str) = serde_json::to_string(&serde_json::Value::Object(env_map)) {
        let _ = db.execute(
            "UPDATE services SET env_json = ?1 WHERE id = ?2",
            rusqlite::params![json_str, service_id],
        );
    }
}

#[allow(dead_code)]
struct ServiceInfo {
    name: String,
    git_repo_url: Option<String>,
    git_branch: Option<String>,
    git_source_id: Option<String>,
    build_strategy: Option<String>,
    compose_content: Option<String>,
    current_image: Option<String>,
}
