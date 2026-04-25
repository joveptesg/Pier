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
        // Supersede any prior in-flight deployments for this service.
        // The old tokio task may still run to completion — `finish_deployment`
        // guards with `status='building'` so it won't overwrite this.
        let _ = db.execute(
            "UPDATE deployments
             SET status = 'cancelled', finished_at = datetime('now')
             WHERE service_id = ?1 AND status IN ('building', 'pending')",
            [&service_id],
        );
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
        Ok(output) => {
            log.push_str(&output);
            flush_log(&state, &deploy_id, &log);
        }
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

    // Resolve registry credentials for this service's project (empty map → no auth).
    let auth_map = {
        let db = state.db.lock().ok();
        db.as_ref()
            .and_then(|d| docker::auth::auth_map_for_service(d, &service_id).ok())
            .unwrap_or_default()
    };
    let build_auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map.clone())
    };

    match strategy {
        "dockerfile" => {
            match build::docker_build(&state.docker, &repo_dir, &image_tag, build_auth.clone())
                .await
            {
                Ok(output) => {
                    log.push_str(&output);
                    flush_log(&state, &deploy_id, &log);
                }
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

            match docker::compose::deploy_stack(
                &stack_name,
                &yaml,
                &state.config,
                build_auth.clone(),
            )
            .await
            {
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
            let stack_env = state
                .config
                .data_dir
                .join("stacks")
                .join(&stack_name)
                .join(".env");
            if stack_env.exists() {
                let _ = tokio::fs::copy(&stack_env, repo_dir.join(".env")).await;
            }

            match tokio::fs::read_to_string(&compose_file).await {
                Ok(yaml) => {
                    let yaml = strip_compose_version(&yaml);
                    // Extract ports before stripping (for port_allocations DB update)
                    extract_and_save_ports(&state, &service_id, &yaml).await;
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

                    // Build and deploy from stack dir — stream output in real-time
                    log.push_str("Building & deploying with docker-compose...\n");
                    flush_log(&state, &deploy_id, &log);

                    // Registry auth: scoped ~/.docker/config.json via DOCKER_CONFIG.
                    let auth_dir = if auth_map.is_empty() {
                        None
                    } else {
                        docker::auth::write_docker_config(&auth_map).ok().flatten()
                    };

                    // Merge stderr into stdout so we get all output in one stream
                    let mut shell_cmd = tokio::process::Command::new("sh");
                    shell_cmd
                        .args([
                            "-c",
                            &format!("docker compose -p {} up -d --build 2>&1", stack_name),
                        ])
                        .current_dir(&stack_dir)
                        .env(
                            "HOME",
                            state
                                .config
                                .data_dir
                                .parent()
                                .unwrap_or(&state.config.data_dir),
                        )
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null());
                    if let Some(dir) = &auth_dir {
                        shell_cmd.env("DOCKER_CONFIG", dir.path());
                    }
                    let child = shell_cmd.spawn();

                    match child {
                        Ok(mut proc) => {
                            use tokio::io::{AsyncBufReadExt, BufReader};

                            if let Some(out) = proc.stdout.take() {
                                let mut reader = BufReader::new(out).lines();
                                let mut line_count = 0u32;
                                while let Ok(Some(line)) = reader.next_line().await {
                                    log.push_str(&line);
                                    log.push('\n');
                                    line_count += 1;
                                    if line_count.is_multiple_of(3) {
                                        flush_log(&state, &deploy_id, &log);
                                    }
                                }
                            }
                            flush_log(&state, &deploy_id, &log);

                            match proc.wait().await {
                                Ok(status) if status.success() => {
                                    // success — continue
                                }
                                Ok(_) => {
                                    log.push_str("Deploy failed (non-zero exit)\n");
                                    finish_deployment(
                                        &state,
                                        &deploy_id,
                                        &service_id,
                                        "failed",
                                        &log,
                                        start,
                                    );
                                    return;
                                }
                                Err(e) => {
                                    log.push_str(&format!("Deploy wait error: {e}\n"));
                                    finish_deployment(
                                        &state,
                                        &deploy_id,
                                        &service_id,
                                        "failed",
                                        &log,
                                        start,
                                    );
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            log.push_str(&format!("Deploy failed: {e}\n"));
                            finish_deployment(
                                &state,
                                &deploy_id,
                                &service_id,
                                "failed",
                                &log,
                                start,
                            );
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

    // Regenerate Traefik domain configs with correct container name and port
    regenerate_domain_configs(&state, &service_id);

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
    state: &Arc<AppState>,
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
        // Only finalise if still in progress — a later redeploy may have
        // marked this row 'cancelled'; don't resurrect it.
        let updated = db
            .execute(
                "UPDATE deployments SET status = ?1, build_log = ?2, duration_secs = ?3, finished_at = datetime('now')
                 WHERE id = ?4 AND status IN ('building', 'pending')",
                rusqlite::params![status, log, duration, deploy_id],
            )
            .unwrap_or(0);
        if updated == 0 {
            // Row was superseded (cancelled) — don't touch the service row either.
            return;
        }
        let _ = db.execute(
            "UPDATE services SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![service_status, service_id],
        );
        // Successful deploy means the container now reflects the current env_json.
        if status == "success" {
            let _ = db.execute(
                "UPDATE services SET env_dirty = 0 WHERE id = ?1",
                [service_id],
            );
        }
    }

    // Fire alert on deployment failure or success
    if status == "failed" || status == "success" {
        let s = state.clone();
        let sid = service_id.to_string();
        let did = deploy_id.to_string();
        let excerpt: String = log.lines().next().unwrap_or("").chars().take(200).collect();
        let service_name: String = state
            .db
            .lock()
            .ok()
            .and_then(|db| {
                db.query_row(
                    "SELECT name FROM services WHERE id = ?1",
                    [service_id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .unwrap_or_else(|| service_id.to_string());
        let status_owned = status.to_string();
        tokio::spawn(async move {
            let (metric, context) = if status_owned == "failed" {
                (
                    "deploy_status",
                    format!("Service: {service_name}\nDeploy {did} failed:\n{excerpt}"),
                )
            } else {
                (
                    "deploy_success",
                    format!("Service: {service_name}\nDeploy {did} succeeded"),
                )
            };
            crate::alerts::hooks::fire_event(&s, metric, Some(&sid), context).await;
        });
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
        if in_services
            && !trimmed.is_empty()
            && trimmed.ends_with(':')
            && (line.starts_with("  ") || line.starts_with('\t'))
            && !line.starts_with("    ")
        {
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
        for (j, line) in lines.iter().enumerate().skip(idx + 1) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if (line.starts_with("  ") || line.starts_with('\t'))
                && !line.starts_with("    ")
                && !line.starts_with("\t\t")
                && trimmed.ends_with(':')
            {
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
        for (j, line) in lines.iter().enumerate().take(end).skip(idx) {
            let trimmed = line.trim();
            if trimmed == "networks:" && (line.starts_with("    ") || line.starts_with("\t\t")) {
                net_start = Some(j);
            } else if net_start.is_some()
                && net_end.is_none()
                && !trimmed.starts_with("- ")
                && !trimmed.is_empty()
            {
                net_end = Some(j);
            }
        }
        if let Some(start) = net_start {
            let end_idx = net_end.unwrap_or(end);
            for _ in start..end_idx {
                if start < lines.len() {
                    lines.remove(start);
                }
            }
        }

        // Find new end after removal
        let mut new_end = lines.len();
        for (j, line) in lines.iter().enumerate().skip(idx + 1) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if (line.starts_with("  ") || line.starts_with('\t'))
                && !line.starts_with("    ")
                && !line.starts_with("\t\t")
                && trimmed.ends_with(':')
            {
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
        for (j, line) in lines.iter().enumerate().skip(start + 1) {
            if !line.starts_with(' ') && !line.starts_with('\t') && !line.trim().is_empty() {
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

/// Parse ports from compose YAML, update `port_allocations` in DB, and
/// reconcile Traefik dynamic config to match.
///
/// Handles formats: "5201:5201", "127.0.0.1:5201:5201", "3000:3000/tcp".
async fn extract_and_save_ports(state: &AppState, service_id: &str, yaml: &str) {
    update_ports_from_compose(state, service_id, yaml);
    if let Err(e) = crate::proxy::sync_tcp_routes_for_service(state, service_id).await {
        // Don't fail the deploy on a Traefik hiccup — port_allocations is the
        // source of truth and the next redeploy will re-converge.
        tracing::warn!("Traefik TCP sync failed for {service_id}: {e}");
    }
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
                let port_str = trimmed
                    .strip_prefix("- ")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                // Remove protocol suffix (/tcp, /udp)
                let port_str = port_str.split('/').next().unwrap_or(port_str);
                // Parse: "host:container" or "ip:host:container"
                let parts: Vec<&str> = port_str.split(':').collect();
                match parts.len() {
                    2 => {
                        if let (Ok(host), Ok(container)) =
                            (parts[0].parse::<u16>(), parts[1].parse::<u16>())
                        {
                            ports.push((host, container));
                        }
                    }
                    3 => {
                        if let (Ok(host), Ok(container)) =
                            (parts[1].parse::<u16>(), parts[2].parse::<u16>())
                        {
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

    if ports.is_empty() {
        return;
    }

    if let Ok(db) = state.db.lock() {
        // Compose `ports:` declarations are the source of truth (Coolify-like behavior).
        // Every redeploy enforces: each declared port → public on its host_port.
        // To disable a port's public exposure, remove it from compose and redeploy.

        let _ = db.execute(
            "DELETE FROM port_allocations WHERE service_id = ?1",
            [service_id],
        );

        for (i, (host_port, container_port)) in ports.iter().enumerate() {
            let port_name = if i == 0 {
                "primary".to_string()
            } else {
                format!("port-{}", i)
            };
            let id = uuid::Uuid::new_v4().to_string();

            // Refuse to claim a public host port already taken by another service.
            // The port stays as Local (is_public=0) so Pier still shows it in the
            // UI; the user can manually toggle once the conflict is resolved.
            let conflict: Option<String> = db
                .query_row(
                    "SELECT service_id FROM port_allocations \
                     WHERE is_public = 1 AND public_port = ?1 AND service_id != ?2 LIMIT 1",
                    rusqlite::params![*host_port as i64, service_id],
                    |row| row.get(0),
                )
                .ok();
            let (is_public, public_port): (i64, Option<i64>) = if let Some(other) = conflict {
                tracing::warn!(
                    "Compose port {host_port} for {service_id} conflicts with public port already held by {other}; staying local"
                );
                (0, None)
            } else {
                (1, Some(*host_port as i64))
            };

            let _ = db.execute(
                "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol, is_public, public_port) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'tcp', ?6, ?7)",
                rusqlite::params![
                    id,
                    service_id,
                    port_name,
                    *host_port as i64,
                    *container_port as i64,
                    is_public,
                    public_port,
                ],
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

    let decrypted = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_content = match serde_json::from_str::<serde_json::Value>(&decrypted) {
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
    };

    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    let env_path = stack_dir.join(".env");
    let _ = tokio::fs::create_dir_all(&stack_dir).await;
    if let Err(e) = tokio::fs::write(&env_path, &env_content).await {
        tracing::warn!("Failed to write .env for {stack_name}: {e}");
    }
    // SEC-006: restrict .env file permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Read .env from stack dir and save to services.env_json if currently empty.
/// This ensures canvas dependency detection works for git-deployed services.
fn persist_env_from_disk(state: &AppState, service_id: &str, stack_name: &str) {
    let db = match state.db.lock() {
        Ok(db) => db,
        Err(_) => return,
    };

    // Check if env_json already has data — decrypt first because the stored
    // value is usually an ENC: ciphertext.
    let current: Option<String> = db
        .query_row(
            "SELECT env_json FROM services WHERE id = ?1",
            [service_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let current_plain = crate::crypto::decrypt_env_json(current.as_deref());
    if current_plain != "{}" && current_plain != "null" && !current_plain.is_empty() {
        return; // Already has env data, don't overwrite
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
        let encrypted = crate::crypto::encrypt_env_json(&json_str);
        let _ = db.execute(
            "UPDATE services SET env_json = ?1 WHERE id = ?2",
            rusqlite::params![encrypted, service_id],
        );
    }
}

/// After deploy, regenerate Traefik configs for all domains of this service.
/// Uses the actual container_id and container_port from DB (now correct after deploy).
fn regenerate_domain_configs(state: &AppState, service_id: &str) {
    let db = match state.db.lock() {
        Ok(db) => db,
        Err(_) => return,
    };

    // Get actual container name
    let container_name: String = db
        .query_row(
            "SELECT container_id FROM services WHERE id = ?1",
            [service_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or_default();

    if container_name.is_empty() {
        return;
    }

    // Get container port (prefer non-management port)
    let http_keywords = ["management", "metrics", "prometheus"];
    let port: Option<i32> = db
        .prepare("SELECT port_name, container_port FROM port_allocations WHERE service_id = ?1")
        .ok()
        .and_then(|mut stmt| {
            let ports: Vec<(String, i32)> = stmt
                .query_map([service_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()?
                .filter_map(|r| r.ok())
                .collect();
            ports
                .iter()
                .find(|(n, _)| !http_keywords.iter().any(|k| n.to_lowercase().contains(k)))
                .or(ports.first())
                .map(|(_, p)| *p)
        });

    let port = match port {
        Some(p) => p,
        None => return,
    };

    let target_url = format!("http://{}:{}", container_name, port);

    // Get all domains for this service
    let domains: Vec<(String, bool)> = db
        .prepare("SELECT domain FROM domains WHERE service_id = ?1")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([service_id], |row| row.get::<_, String>(0))
                .unwrap_or_else(|_| panic!())
                .filter_map(|r| r.ok())
                .map(|d| (d, true))
                .collect()
        })
        .unwrap_or_default();

    if domains.is_empty() {
        return;
    }

    if let Err(e) = crate::proxy::config::regenerate_service_config(
        &state.config.data_dir,
        service_id,
        &domains,
        &target_url,
    ) {
        tracing::warn!("Failed to regenerate domain configs for {service_id}: {e}");
    } else {
        tracing::info!(
            "Regenerated Traefik configs for {service_id}: {} → {target_url}",
            domains
                .iter()
                .map(|(d, _)| d.as_str())
                .collect::<Vec<_>>()
                .join(", ")
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
