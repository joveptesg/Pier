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
                    // Auto-wire `.env` (which Pier writes from the UI's env_json) into
                    // every service so UI-defined vars reach the container by default.
                    // `environment:` in the user's compose still wins for explicit overrides.
                    let yaml = inject_env_file_into_services(&yaml);

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

/// Clone the service's git repo (HEAD of the configured branch) into a temp
/// directory, read `docker-compose.yml` from the clone, remove the temp dir,
/// and return the file contents. Used by the "Reload compose from git" UI
/// action so users can see and re-sync the live file without rebuilding the
/// running container.
pub async fn fetch_compose_from_git(state: &AppState, service_id: &str) -> Result<String> {
    let svc = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, git_repo_url, git_branch, git_source_id, build_strategy, compose_content, image
             FROM services WHERE id = ?1",
            [service_id],
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
        )?
    };

    if svc.git_repo_url.as_deref().unwrap_or("").is_empty() {
        return Err(anyhow::anyhow!("Service has no git_repo_url configured"));
    }

    let clone_url = resolve_clone_url(state, &svc).await?;
    let branch = svc.git_branch.as_deref().unwrap_or("main");

    let tmp = state
        .config
        .data_dir
        .join("tmp")
        .join(format!("compose-fetch-{}", uuid::Uuid::new_v4()));
    build::clone_repo(&clone_url, branch, &tmp).await?;

    let compose_path = tmp.join("docker-compose.yml");
    let yaml = tokio::fs::read_to_string(&compose_path).await.map_err(|e| {
        anyhow::anyhow!("docker-compose.yml not found in repo (branch {branch}): {e}")
    });
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    yaml
}

/// Re-read the live docker-compose.yml from git and re-sync the service's
/// `port_allocations` + Traefik dynamic routes — without touching the running
/// container. Returns the compose YAML on success so the UI can refresh its
/// preview in one round-trip.
pub async fn reload_compose_ports(state: &AppState, service_id: &str) -> Result<String> {
    let yaml = fetch_compose_from_git(state, service_id).await?;
    let yaml_stripped_version = strip_compose_version(&yaml);
    update_ports_from_compose(state, service_id, &yaml_stripped_version);
    crate::proxy::sync_tcp_routes_for_service(state, service_id).await?;
    Ok(yaml)
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

/// One compose `services:` entry, distilled to the bits Pier cares about
/// for routing decisions (container DNS name + ports).
#[derive(Debug, Clone)]
pub struct ComposeService {
    /// YAML key under `services:`.
    pub name: String,
    /// Explicit `container_name:` value if set. Empty when not specified —
    /// callers that need the runtime name should fall back to `pier-{slug}`
    /// or query `docker compose ps`.
    #[allow(dead_code)] // consumed by Patch C2 (per-service domain wiring)
    pub container_name: String,
    /// Resolved (host, container) port pairs after `${VAR}` substitution.
    pub ports: Vec<(u16, u16)>,
}

/// Lightweight parser for the `services:` block of a docker-compose file.
/// Only what Pier needs: per-service `container_name` and `ports`. We avoid
/// pulling in a full YAML crate because (a) the format we care about is a
/// stable subset and (b) the existing line-by-line parser already worked for
/// `ports:` — this just generalises it to track the enclosing service.
///
/// Indentation rules (matching how docker-compose files are conventionally
/// written; both 2-space and 4-space styles are supported):
///   services:                ← top-level (column 0)
///     <name>:                ← service name (one indent)
///       container_name: ...  ← service property (two indents)
///       ports:               ← service property
///         - "host:container" ← list item (three indents)
pub fn parse_compose_services(
    yaml: &str,
    env: &std::collections::HashMap<String, String>,
) -> Vec<ComposeService> {
    let mut services: Vec<ComposeService> = Vec::new();
    let mut in_services = false;
    let mut service_indent: Option<usize> = None; // indent of `<name>:` rows
    let mut prop_indent: Option<usize> = None; // indent of `container_name:` / `ports:` rows
    let mut in_ports = false;
    let mut ports_item_indent: Option<usize> = None;

    let push_current = |services: &mut Vec<ComposeService>,
                        name: &mut Option<String>,
                        container: &mut String,
                        ports: &mut Vec<(u16, u16)>| {
        if let Some(n) = name.take() {
            services.push(ComposeService {
                name: n,
                container_name: std::mem::take(container),
                ports: std::mem::take(ports),
            });
        }
    };

    let mut current_name: Option<String> = None;
    let mut current_container = String::new();
    let mut current_ports: Vec<(u16, u16)> = Vec::new();

    for raw_line in yaml.lines() {
        // Strip trailing comment (after a `#` preceded by whitespace).
        let line: &str = match raw_line.find(" #") {
            Some(idx) => &raw_line[..idx],
            None => raw_line,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();

        // Top-level key: leaves any previous services: scope.
        if indent == 0 {
            push_current(
                &mut services,
                &mut current_name,
                &mut current_container,
                &mut current_ports,
            );
            in_services = trimmed == "services:";
            service_indent = None;
            prop_indent = None;
            in_ports = false;
            ports_item_indent = None;
            continue;
        }

        if !in_services {
            continue;
        }

        // First indented line under `services:` establishes the service-level indent.
        if service_indent.is_none() {
            service_indent = Some(indent);
        }
        let svc_ind = service_indent.unwrap();

        if indent == svc_ind && trimmed.ends_with(':') {
            // New service definition.
            push_current(
                &mut services,
                &mut current_name,
                &mut current_container,
                &mut current_ports,
            );
            current_name = Some(trimmed.trim_end_matches(':').to_string());
            current_container.clear();
            current_ports.clear();
            prop_indent = None;
            in_ports = false;
            ports_item_indent = None;
            continue;
        }

        if current_name.is_none() {
            continue;
        }

        // First indented line *inside* a service block establishes the prop indent.
        if prop_indent.is_none() && indent > svc_ind {
            prop_indent = Some(indent);
        }
        let pi = prop_indent.unwrap_or(svc_ind + 2);

        if indent == pi {
            // Service property.
            in_ports = false;
            if let Some(rest) = trimmed.strip_prefix("container_name:") {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                current_container = val.to_string();
            } else if trimmed == "ports:" {
                in_ports = true;
                ports_item_indent = None;
            }
            continue;
        }

        if in_ports && indent > pi && trimmed.starts_with("- ") {
            if ports_item_indent.is_none() {
                ports_item_indent = Some(indent);
            }
            if Some(indent) == ports_item_indent {
                let port_str = trimmed
                    .strip_prefix("- ")
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                let substituted = substitute_compose_vars(port_str, env);
                let port_str = substituted.split('/').next().unwrap_or(&substituted);
                let parts: Vec<&str> = port_str.split(':').collect();
                let parsed = match parts.len() {
                    2 => parts[0]
                        .parse::<u16>()
                        .ok()
                        .zip(parts[1].parse::<u16>().ok()),
                    3 => parts[1]
                        .parse::<u16>()
                        .ok()
                        .zip(parts[2].parse::<u16>().ok()),
                    1 => parts[0].parse::<u16>().ok().map(|p| (p, p)),
                    _ => None,
                };
                if let Some(p) = parsed {
                    current_ports.push(p);
                }
            }
            continue;
        }
    }

    push_current(
        &mut services,
        &mut current_name,
        &mut current_container,
        &mut current_ports,
    );
    services
}

fn update_ports_from_compose(state: &AppState, service_id: &str, yaml: &str) {
    // Resolve `${VAR}` / `${VAR:-default}` like docker-compose does, using
    // the service's env_json as the source for VAR values. Without this,
    // entries like `${PORT:-6031}:6031` would never parse and the service
    // would keep stale port_allocations.
    let env_map = load_env_map(state, service_id);

    let services = parse_compose_services(yaml, &env_map);

    // Flatten while remembering which compose-service each port belongs to.
    // Single-service composes preserve legacy port_name (`primary`, `port-1`)
    // for backward-compat; multi-service composes use NULL/None compose_service
    // tagging so the per-service domain wiring (Patch C) can resolve upstreams.
    let multi_service = services.len() > 1;
    let mut flat: Vec<(Option<String>, String, u16, u16)> = Vec::new();
    for svc in &services {
        for (i, (host_port, container_port)) in svc.ports.iter().enumerate() {
            let port_name = if i == 0 {
                "primary".to_string()
            } else {
                format!("port-{i}")
            };
            let compose_svc = if multi_service {
                Some(svc.name.clone())
            } else {
                None
            };
            flat.push((compose_svc, port_name, *host_port, *container_port));
        }
    }

    if flat.is_empty() {
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

        for (compose_svc, port_name, host_port, container_port) in &flat {
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
                "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol, is_public, public_port, compose_service) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'tcp', ?6, ?7, ?8)",
                rusqlite::params![
                    id,
                    service_id,
                    port_name,
                    *host_port as i64,
                    *container_port as i64,
                    is_public,
                    public_port,
                    compose_svc,
                ],
            );
        }

        // Update services.port with the first host port (legacy single-port
        // field; UI prefers port_allocations now, but other code paths still
        // read this).
        if let Some((_, _, host_port, _)) = flat.first() {
            let _ = db.execute(
                "UPDATE services SET port = ?1 WHERE id = ?2",
                rusqlite::params![*host_port as i64, service_id],
            );
        }

        let summary: Vec<(Option<&str>, u16, u16)> = flat
            .iter()
            .map(|(svc, _, h, c)| (svc.as_deref(), *h, *c))
            .collect();
        tracing::info!("Updated ports from compose for {service_id}: {summary:?}");
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

/// Inject `env_file: - .env` into every service block of a docker-compose
/// YAML that does not already declare `env_file:`. Compose merges `env_file`
/// values *under* anything in `environment:`, so this gives users a
/// "Environment Variables in the Pier UI flow into the container by default,
/// `environment:` in compose still wins for explicit overrides" experience.
///
/// Services that already specify their own `env_file:` (in any form — string,
/// list, single-line) are left untouched: an explicit user choice in the
/// compose file always takes priority over Pier's auto-injection.
///
/// The function preserves the file's existing indentation style (2-space vs
/// 4-space) by inferring per-service property indent from the first indented
/// line inside each service block, falling back to `service_indent + 2`.
fn inject_env_file_into_services(yaml: &str) -> String {
    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();

    // Locate the top-level `services:` key.
    let services_idx = match lines
        .iter()
        .position(|l| l.trim() == "services:" && !l.starts_with(' ') && !l.starts_with('\t'))
    {
        Some(i) => i,
        None => return yaml.to_string(),
    };

    // Determine the indent shared by every direct child of `services:`.
    let service_indent = lines
        .iter()
        .skip(services_idx + 1)
        .find_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let indent = line.len() - line.trim_start().len();
            if indent == 0 {
                return Some(0); // sentinel: another top-level key, no services
            }
            Some(indent)
        })
        .unwrap_or(0);
    if service_indent == 0 {
        return yaml.to_string();
    }

    // Collect (start, end) ranges for each service block. `end` is exclusive.
    let mut service_ranges: Vec<(usize, usize)> = Vec::new();
    let mut current_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().skip(services_idx + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            // New top-level key — services: section ends here.
            if let Some(start) = current_start.take() {
                service_ranges.push((start, i));
            }
            break;
        }
        if indent == service_indent && trimmed.ends_with(':') {
            // Close the previous service before opening a new one.
            if let Some(start) = current_start.take() {
                service_ranges.push((start, i));
            }
            current_start = Some(i);
        }
    }
    if let Some(start) = current_start.take() {
        service_ranges.push((start, lines.len()));
    }

    // Process in reverse so earlier indices stay valid as we insert lines.
    for (start, end) in service_ranges.into_iter().rev() {
        // Infer this service's property indent from the first indented body line.
        let prop_indent = lines[start + 1..end]
            .iter()
            .find_map(|line| {
                if line.trim().is_empty() {
                    return None;
                }
                let indent = line.len() - line.trim_start().len();
                if indent > service_indent {
                    Some(indent)
                } else {
                    None
                }
            })
            .unwrap_or(service_indent + 2);

        // If the user already set env_file: at this service's prop level, skip.
        let has_env_file = lines[start + 1..end].iter().any(|l| {
            let indent = l.len() - l.trim_start().len();
            indent == prop_indent && l.trim_start().starts_with("env_file:")
        });
        if has_env_file {
            continue;
        }

        // Insert just before any trailing blank lines that separate services.
        let mut insert_at = end;
        while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
            insert_at -= 1;
        }

        let pad = " ".repeat(prop_indent);
        lines.insert(insert_at, format!("{pad}env_file:"));
        lines.insert(insert_at + 1, format!("{pad}  - .env"));
    }

    lines.join("\n")
}

/// Load a service's env vars from `services.env_json` into a flat map for
/// docker-compose-style `${VAR}` substitution. Returns an empty map on any
/// error (caller treats unset vars as empty).
pub(crate) fn load_env_map(
    state: &AppState,
    service_id: &str,
) -> std::collections::HashMap<String, String> {
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
    match serde_json::from_str::<serde_json::Value>(&decrypted) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, s)
            })
            .collect(),
        _ => std::collections::HashMap::new(),
    }
}

/// Substitute docker-compose-style variable references in a single token.
/// Supports `${VAR}` (empty if unset) and `${VAR:-default}` (default if unset).
/// Other `$` usages are left as-is.
fn substitute_compose_vars(s: &str, env: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close_off) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                let inner = &s[i + 2..i + 2 + close_off];
                let resolved = if let Some(idx) = inner.find(":-") {
                    let var = &inner[..idx];
                    let default = &inner[idx + 2..];
                    env.get(var).cloned().unwrap_or_else(|| default.to_string())
                } else {
                    env.get(inner).cloned().unwrap_or_default()
                };
                out.push_str(&resolved);
                i += 2 + close_off + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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

/// Decrypt and format a stored `services.env_json` value as the body of a
/// `.env` file (plaintext `KEY=VALUE` lines, one per line, no trailing
/// newline). Pure function — split out so the encryption/serialization logic
/// is unit-testable without an `AppState`.
pub(crate) fn env_json_to_env_content(stored_env_json: Option<&str>) -> String {
    let decrypted = crate::crypto::decrypt_env_json(stored_env_json);
    match serde_json::from_str::<serde_json::Value>(&decrypted) {
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

/// Write an .env file to the stack directory from the service's env_json.
///
/// Decrypts `services.env_json` (handles both encrypted `ENC:...` and legacy
/// plaintext) and materializes plaintext `KEY=VALUE` lines to
/// `{stack_dir}/.env`. Compose reads this file only on `compose up`; running
/// containers keep the env baked in at create time, so we don't need to
/// regenerate on Docker daemon restart.
///
/// Writes are atomic (`.env.tmp` + `rename`) so concurrent redeploys of the
/// same service can never observe a partial file. Permissions are set on the
/// temp file before rename to avoid a window with default umask on the final
/// path.
pub(crate) async fn write_env_file(state: &AppState, service_id: &str, stack_name: &str) {
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

    if env_json.as_deref().is_some_and(|s| s.starts_with("ENC:"))
        && crate::crypto::decrypt_env_json(env_json.as_deref()) == "{}"
    {
        tracing::warn!(
            "env_json decrypt returned empty for service {service_id}; \
             check PIER_SECRET — container will start with no env"
        );
    }
    let env_content = env_json_to_env_content(env_json.as_deref());

    let stack_dir = state.config.data_dir.join("stacks").join(stack_name);
    let env_path = stack_dir.join(".env");
    let tmp_path = stack_dir.join(".env.tmp");
    let _ = tokio::fs::create_dir_all(&stack_dir).await;

    if let Err(e) = tokio::fs::write(&tmp_path, &env_content).await {
        tracing::warn!("Failed to write .env.tmp for {stack_name}: {e}");
        return;
    }
    // SEC-006: restrict permissions on the tmp file BEFORE rename — otherwise
    // there's a brief window where the final .env exists with the default umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, &env_path).await {
        tracing::warn!("Failed to rename .env.tmp -> .env for {stack_name}: {e}");
        let _ = tokio::fs::remove_file(&tmp_path).await;
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

#[cfg(test)]
mod tests {
    use super::{env_json_to_env_content, inject_env_file_into_services};
    use crate::crypto::encrypt_env_json;

    #[test]
    fn inject_env_file_adds_to_service_without_it() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n";
        let out = inject_env_file_into_services(yaml);
        assert!(out.contains("env_file:"));
        assert!(out.contains("- .env"));
        assert!(out.contains("image: foo:bar"));
    }

    #[test]
    fn inject_env_file_skips_service_with_existing_env_file() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n    env_file:\n      - custom.env\n";
        let out = inject_env_file_into_services(yaml);
        assert!(out.contains("custom.env"));
        // Only one env_file: line should remain — the user's.
        assert_eq!(out.matches("env_file:").count(), 1);
        assert!(!out.contains("- .env"));
    }

    #[test]
    fn inject_env_file_handles_multi_service_compose() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n  worker:\n    image: baz:qux\n";
        let out = inject_env_file_into_services(yaml);
        assert_eq!(out.matches("env_file:").count(), 2);
        assert_eq!(out.matches("- .env").count(), 2);
    }

    #[test]
    fn inject_env_file_preserves_environment_section() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n    environment:\n      - NODE_ENV=production\n      - DB_HOST=${DB_HOST}\n";
        let out = inject_env_file_into_services(yaml);
        assert!(out.contains("environment:"));
        assert!(out.contains("NODE_ENV=production"));
        assert!(out.contains("DB_HOST=${DB_HOST}"));
        assert!(out.contains("env_file:"));
        assert!(out.contains("- .env"));
    }

    #[test]
    fn inject_env_file_handles_4_space_indent() {
        let yaml = "services:\n    backend:\n        image: foo:bar\n";
        let out = inject_env_file_into_services(yaml);
        // env_file should be at the same prop indent (8 spaces) as image:.
        assert!(out.contains("        env_file:"));
        assert!(out.contains("        - .env"));
    }

    #[test]
    fn inject_env_file_does_not_leak_into_top_level_networks() {
        let yaml = "services:\n  backend:\n    image: foo:bar\nnetworks:\n  pier-net:\n    external: true\n";
        let out = inject_env_file_into_services(yaml);
        // env_file inserted *inside* backend, not under networks:
        let env_file_pos = out.find("env_file:").expect("env_file should be present");
        let networks_pos = out
            .find("\nnetworks:")
            .expect("networks: should be present");
        assert!(env_file_pos < networks_pos);
        assert_eq!(out.matches("env_file:").count(), 1);
    }

    #[test]
    fn inject_env_file_noop_without_services_block() {
        let yaml = "version: '3'\nnetworks:\n  pier-net:\n    external: true\n";
        let out = inject_env_file_into_services(yaml);
        assert!(!out.contains("env_file:"));
    }

    #[test]
    fn plaintext_json_renders_as_env_lines() {
        let json = r#"{"S3_RU_ENDPOINT":"https://s3.example.com","KEY":"VAL"}"#;
        let out = env_json_to_env_content(Some(json));
        assert!(out.contains("S3_RU_ENDPOINT=https://s3.example.com"));
        assert!(out.contains("KEY=VAL"));
        assert!(!out.contains("ENC:"));
    }

    #[test]
    fn encrypted_json_round_trips_to_env_lines() {
        let json = r#"{"S3_RU_ENDPOINT":"https://s3.example.com","KEY":"VAL"}"#;
        let encrypted = encrypt_env_json(json);
        let out = env_json_to_env_content(Some(&encrypted));
        assert!(out.contains("S3_RU_ENDPOINT=https://s3.example.com"));
        assert!(out.contains("KEY=VAL"));
    }

    #[test]
    fn null_or_empty_yields_empty_content() {
        assert!(env_json_to_env_content(None).is_empty());
        assert!(env_json_to_env_content(Some("")).is_empty());
        assert!(env_json_to_env_content(Some("null")).is_empty());
    }

    #[test]
    fn corrupted_ciphertext_yields_empty_not_panic() {
        let out = env_json_to_env_content(Some("ENC:notbase64:alsonotbase64"));
        assert!(out.is_empty());
    }
}
