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
                    // Strip what the user wrote in `ports:` — we authoritatively
                    // re-emit it from `port_allocations` so the operator's
                    // public/private toggle state survives `git pull`/redeploy.
                    let yaml = strip_compose_ports(&yaml);
                    // Re-inject `ports:` from the DB. Each row becomes one
                    // host binding: `0.0.0.0:public_port:container_port` if
                    // toggled public, `127.0.0.1:host_port:container_port`
                    // otherwise (so the host can still reach it for ops).
                    // This means `docker compose up` brings up the container
                    // already in its desired published state — no post-deploy
                    // Bollard recreate needed.
                    let yaml = inject_ports_from_db(&state, &service_id, &yaml);
                    // Auto-wire `.env` (which Pier writes from the UI's env_json) into
                    // every service so UI-defined vars reach the container by default.
                    // `environment:` in the user's compose still wins for explicit overrides.
                    let yaml = inject_env_file_into_services(&yaml);
                    // Mesh-DNS: when WireGuard mesh is active, every container gets
                    // `extra_hosts:` entries mapping `{peer}.mesh` to that peer's
                    // private IP. No-op when mesh is disabled or no peers are
                    // `active`, so non-mesh deployments are byte-identical.
                    let mesh_hosts = mesh_hosts_for_inject(&state);
                    let yaml = inject_mesh_extra_hosts_into_services(&yaml, &mesh_hosts);

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

    // Regenerate Traefik domain configs from the domains table — multi-target
    // aware so per-compose-service domains route to the right container.
    if let Err(e) = crate::api::domains::regenerate_for_service(&state, &service_id) {
        tracing::warn!("Failed to regenerate Traefik config for {service_id}: {e}");
    }

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
/// `port_allocations` — without touching the running container. Returns the
/// compose YAML on success so the UI can refresh its preview in one round-trip.
///
/// Public TCP exposure is owned by the compose file itself (Docker port
/// bindings) since the Traefik TCP routing path was removed; no proxy sync
/// needed here — the next `docker compose up` will pick up the new ports.
pub async fn reload_compose_ports(state: &AppState, service_id: &str) -> Result<String> {
    let yaml = fetch_compose_from_git(state, service_id).await?;
    let yaml_stripped_version = strip_compose_version(&yaml);
    update_ports_from_compose(state, service_id, &yaml_stripped_version);
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

/// Parse ports from compose YAML and update `port_allocations` in DB.
///
/// Handles formats: "5201:5201", "127.0.0.1:5201:5201", "3000:3000/tcp".
/// Public TCP exposure now lives in the compose file's `ports:` lines (direct
/// Docker port binding), so no Traefik sync is needed — `docker compose up`
/// already applied any new bindings before this is called.
async fn extract_and_save_ports(state: &AppState, service_id: &str, yaml: &str) {
    update_ports_from_compose(state, service_id, yaml);
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
        // Compose `ports:` declarations describe what *exists*. The operator
        // decides what's *public* via the UI toggle. So on redeploy we preserve
        // any previously-set `is_public`/`public_port` for ports the operator
        // had already published, keyed by `(compose_service, container_port)`.
        // Newly-declared ports default to private — initial-deploy invariant
        // "nothing is publicly exposed unless explicitly toggled."
        let prev_state: std::collections::HashMap<(Option<String>, u16), (bool, Option<u16>)> = {
            let mut prev = std::collections::HashMap::new();
            if let Ok(mut stmt) = db.prepare(
                "SELECT compose_service, container_port, is_public, public_port \
                 FROM port_allocations WHERE service_id = ?1",
            ) {
                let rows = stmt.query_map([service_id], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)? as u16,
                        row.get::<_, i64>(2)? != 0,
                        row.get::<_, Option<i64>>(3)?.map(|p| p as u16),
                    ))
                });
                if let Ok(iter) = rows {
                    for r in iter.flatten() {
                        prev.insert((r.0, r.1), (r.2, r.3));
                    }
                }
            }
            prev
        };

        let _ = db.execute(
            "DELETE FROM port_allocations WHERE service_id = ?1",
            [service_id],
        );

        for (compose_svc, port_name, host_port, container_port) in &flat {
            let id = uuid::Uuid::new_v4().to_string();

            // Carry over the operator's previous toggle decision when this
            // exact `(compose_service, container_port)` was already in the
            // table. Anything not found → default private.
            let key = (compose_svc.clone(), *container_port);
            let (mut is_public_flag, mut pp_value): (bool, Option<u16>) =
                prev_state.get(&key).copied().unwrap_or((false, None));

            // If the carry-over public_port now collides with another
            // service's public_port (e.g. user changed compose and freed
            // the host port for someone else in between), revert to private
            // and let the operator re-toggle once they pick a new value.
            if is_public_flag {
                if let Some(pp) = pp_value {
                    let conflict: Option<String> = db
                        .query_row(
                            "SELECT service_id FROM port_allocations \
                             WHERE is_public = 1 AND public_port = ?1 AND service_id != ?2 LIMIT 1",
                            rusqlite::params![pp as i64, service_id],
                            |row| row.get(0),
                        )
                        .ok();
                    if let Some(other) = conflict {
                        tracing::warn!(
                            "Carry-over public port {pp} for {service_id}/{container_port} \
                             conflicts with {other}; reverting to private"
                        );
                        is_public_flag = false;
                        pp_value = None;
                    }
                }
            }

            // `port_allocations.host_port` has a global UNIQUE constraint.
            // If another service already holds our declared host_port, find a
            // free fallback in the 10000-19999 pool so this row isn't silently
            // dropped (which is exactly what hid port-1=1883 from the UI on
            // myhome-backend after a redeploy). The container's internal
            // container_port is unchanged; only the host-side allocation slot
            // moves to a free slot.
            let mut effective_host_port: u16 = *host_port;
            let owner: Option<String> = db
                .query_row(
                    "SELECT service_id FROM port_allocations \
                     WHERE host_port = ?1 AND service_id != ?2 LIMIT 1",
                    rusqlite::params![effective_host_port as i64, service_id],
                    |row| row.get(0),
                )
                .ok();
            if let Some(other) = owner {
                let fallback = (10000u16..20000u16).find(|p| {
                    db.query_row(
                        "SELECT 1 FROM port_allocations WHERE host_port = ?1 LIMIT 1",
                        rusqlite::params![*p as i64],
                        |row| row.get::<_, i64>(0),
                    )
                    .is_err()
                });
                match fallback {
                    Some(p) => {
                        tracing::warn!(
                            "Host port {effective_host_port} (declared for {service_id}/\
                             {port_name}=container:{container_port}) is held by service {other}; \
                             allocating fallback host_port={p}. Container will still listen on \
                             {container_port} internally; the UI will show :{p} as the host \
                             alloc slot. Public-port toggle remains independent."
                        );
                        effective_host_port = p;
                    }
                    None => {
                        tracing::error!(
                            "Host port {effective_host_port} for {service_id}/{port_name} is \
                             held by {other} and no free fallback in 10000-19999 — this port \
                             will be missing from the UI."
                        );
                        continue;
                    }
                }
            }

            match db.execute(
                "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol, is_public, public_port, compose_service) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'tcp', ?6, ?7, ?8)",
                rusqlite::params![
                    id,
                    service_id,
                    port_name,
                    effective_host_port as i64,
                    *container_port as i64,
                    is_public_flag as i64,
                    pp_value.map(|p| p as i64),
                    compose_svc,
                ],
            ) {
                Ok(_) => {}
                Err(e) => {
                    // No more silent failures — surface the cause so the next
                    // missing-port-in-UI bug is one journalctl away.
                    tracing::error!(
                        "port_allocations INSERT for {service_id}/{port_name} \
                         (host={effective_host_port} container={container_port}) failed: {e}"
                    );
                }
            }
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

/// Re-emit every service's `ports:` block from `port_allocations` rows so
/// `docker compose up` brings up the container already published exactly the
/// way Pier wants.
///
/// Only `is_public=1` rows produce a host binding (`0.0.0.0:public_port:container_port`).
/// Private rows stay reachable through `pier-net` by container name and need
/// no host binding — mirroring how Coolify's "Ports Mappings" works (and
/// avoiding the duplicate-bind crash that hits docker compose when both
/// `127.0.0.1:N:N` and `0.0.0.0:N:N` are emitted for the same host port).
///
/// Multi-service compose stacks route rows by `port_allocations.compose_service`;
/// single-service stacks (compose_service IS NULL) drop everything into the
/// only block. Services with no public ports get no `ports:` block at all.
///
/// Pair with `strip_compose_ports`: strip removes whatever the user wrote
/// (it's no longer authoritative), this re-emits from the DB. The pair makes
/// the operator's toggle state survive `git pull` + redeploy without a
/// post-deploy Bollard recreate.
fn inject_ports_from_db(state: &AppState, service_id: &str, yaml: &str) -> String {
    // (compose_service, container_port, is_public, host_port, public_port)
    type Row = (Option<String>, u16, bool, u16, Option<u16>);
    let rows: Vec<Row> = {
        let Ok(db) = state.db.lock() else {
            return yaml.to_string();
        };
        let Ok(mut stmt) = db.prepare(
            "SELECT compose_service, container_port, is_public, host_port, public_port \
             FROM port_allocations WHERE service_id = ?1 ORDER BY rowid",
        ) else {
            return yaml.to_string();
        };
        let iter = stmt.query_map([service_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)? as u16,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(3)? as u16,
                row.get::<_, Option<i64>>(4)?.map(|p| p as u16),
            ))
        });
        match iter {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => return yaml.to_string(),
        }
    };

    if rows.is_empty() {
        return yaml.to_string();
    }

    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();

    // Locate top-level `services:`.
    let services_idx = match lines
        .iter()
        .position(|l| l.trim() == "services:" && !l.starts_with(' ') && !l.starts_with('\t'))
    {
        Some(i) => i,
        None => return yaml.to_string(),
    };

    // Service-name indent.
    let service_indent = lines
        .iter()
        .skip(services_idx + 1)
        .find_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let indent = line.len() - line.trim_start().len();
            if indent == 0 {
                return Some(0);
            }
            Some(indent)
        })
        .unwrap_or(0);
    if service_indent == 0 {
        return yaml.to_string();
    }

    // Collect (name, start, end) for each service block.
    let mut service_ranges: Vec<(String, usize, usize)> = Vec::new();
    let mut current: Option<(String, usize)> = None;
    for (i, line) in lines.iter().enumerate().skip(services_idx + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            if let Some((n, s)) = current.take() {
                service_ranges.push((n, s, i));
            }
            break;
        }
        if indent == service_indent && trimmed.ends_with(':') {
            if let Some((n, s)) = current.take() {
                service_ranges.push((n, s, i));
            }
            current = Some((trimmed.trim_end_matches(':').trim().to_string(), i));
        }
    }
    if let Some((n, s)) = current.take() {
        service_ranges.push((n, s, lines.len()));
    }

    if service_ranges.is_empty() {
        return yaml.to_string();
    }

    let only_one_service = service_ranges.len() == 1;

    // Walk services in reverse so earlier indices stay valid as we splice in
    // the `ports:` block.
    for (svc_name, start, end) in service_ranges.iter().rev() {
        let svc_rows: Vec<&Row> = rows
            .iter()
            .filter(|(cs, _, _, _, _)| match cs {
                Some(name) => name == svc_name,
                None => only_one_service,
            })
            .collect();

        if svc_rows.is_empty() {
            continue;
        }

        // Property indent of this block.
        let prop_indent = (start + 1..*end)
            .find_map(|i| {
                let line = &lines[i];
                if line.trim().is_empty() {
                    return None;
                }
                let ind = line.len() - line.trim_start().len();
                if ind > service_indent {
                    Some(ind)
                } else {
                    None
                }
            })
            .unwrap_or(service_indent + 2);

        let key_pad = " ".repeat(prop_indent);
        let item_pad = " ".repeat(prop_indent + 2);

        // Public ports only — emit one `0.0.0.0:public:container` binding
        // per is_public=1 row. Private ports get NO host binding (the
        // container is reachable from `pier-net` by its service name, and
        // a redundant `127.0.0.1:host:container` would collide with the
        // 0.0.0.0 binding when public_port == host_port — that's exactly
        // what blew up the redeploy with "address already in use").
        let mut public_lines: Vec<String> = Vec::new();
        for (_, container_port, is_public, _host_port, public_port) in &svc_rows {
            if !*is_public {
                continue;
            }
            let Some(pp) = public_port else { continue };
            public_lines.push(format!("{item_pad}- \"0.0.0.0:{pp}:{container_port}\""));
        }
        if public_lines.is_empty() {
            // No public ports for this service — skip emitting a `ports:`
            // block altogether so the compose stays clean.
            continue;
        }
        let mut block = vec![format!("{key_pad}ports:")];
        block.extend(public_lines);

        // Insert just before any trailing blank lines at the end of the
        // service block, mirroring `inject_env_file_into_services`.
        let mut insert_at = *end;
        while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
            insert_at -= 1;
        }
        let _: Vec<String> = lines.splice(insert_at..insert_at, block).collect();
    }

    let mut out = lines.join("\n");
    if yaml.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
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

/// Inject `extra_hosts:` entries into every `services:` block so the
/// container resolves Pier mesh peer names (`vps1.mesh`, `vps2.mesh`,
/// …) to their private mesh IPs. This is how an app deployed on one
/// node reaches a sibling app on another through the WireGuard tunnel
/// without the operator hard-coding `10.42.0.x` in env vars.
///
/// `hosts` is `(hostname, ip)` pairs already sanitised by the caller.
/// Pass an empty slice to leave the YAML untouched (e.g. mesh
/// disabled, no active peers, or the deployment isn't supposed to
/// participate in mesh-DNS).
///
/// Services that already declare an `extra_hosts:` key are left alone
/// — overwriting would silently drop the operator's entries. The
/// trade-off is that those services don't get mesh-DNS automatically;
/// they can include the entries themselves if they want both.
pub fn inject_mesh_extra_hosts_into_services(yaml: &str, hosts: &[(String, String)]) -> String {
    if hosts.is_empty() {
        return yaml.to_string();
    }
    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();

    // Locate top-level `services:` — same scanning approach as
    // inject_env_file_into_services. Kept inline rather than
    // refactored into a shared helper because the structural-edit
    // pattern is small and clearer when read top-to-bottom.
    let services_idx = match lines
        .iter()
        .position(|l| l.trim() == "services:" && !l.starts_with(' ') && !l.starts_with('\t'))
    {
        Some(i) => i,
        None => return yaml.to_string(),
    };

    let service_indent = lines
        .iter()
        .skip(services_idx + 1)
        .find_map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            let indent = line.len() - line.trim_start().len();
            if indent == 0 {
                return Some(0);
            }
            Some(indent)
        })
        .unwrap_or(0);
    if service_indent == 0 {
        return yaml.to_string();
    }

    let mut service_ranges: Vec<(usize, usize)> = Vec::new();
    let mut current_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().skip(services_idx + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            if let Some(start) = current_start.take() {
                service_ranges.push((start, i));
            }
            break;
        }
        if indent == service_indent && trimmed.ends_with(':') {
            if let Some(start) = current_start.take() {
                service_ranges.push((start, i));
            }
            current_start = Some(i);
        }
    }
    if let Some(start) = current_start.take() {
        service_ranges.push((start, lines.len()));
    }

    for (start, end) in service_ranges.into_iter().rev() {
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

        let has_extra_hosts = lines[start + 1..end].iter().any(|l| {
            let indent = l.len() - l.trim_start().len();
            indent == prop_indent && l.trim_start().starts_with("extra_hosts:")
        });
        if has_extra_hosts {
            continue;
        }

        let mut insert_at = end;
        while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
            insert_at -= 1;
        }

        let pad = " ".repeat(prop_indent);
        lines.insert(insert_at, format!("{pad}extra_hosts:"));
        for (i, (host, ip)) in hosts.iter().enumerate() {
            lines.insert(insert_at + 1 + i, format!("{pad}  - \"{host}:{ip}\""));
        }
    }

    lines.join("\n")
}

/// Build the list of mesh peer hostnames Pier should inject into every
/// deployed stack. Returns an empty vec when mesh is disabled or no
/// peers have reached `status='active'` — the caller is expected to
/// treat that as "skip injection".
///
/// Hostname normalisation: `lower(server.name)`, swap whitespace +
/// non-`[a-z0-9.-]` characters for `-`, collapse runs, suffix with
/// `.mesh`. Predictable and reversible enough for the operator to
/// guess what to put in their app env (`http://vps1-master.mesh:8080`)
/// before they go look at the dashboard.
pub fn mesh_hosts_for_inject(state: &crate::state::AppState) -> Vec<(String, String)> {
    let db = match state.db.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let enabled: i64 = db
        .query_row(
            "SELECT enabled FROM wireguard_config WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if enabled != 1 {
        return Vec::new();
    }
    let mut stmt = match db.prepare(
        "SELECT s.name, wp.assigned_ip
         FROM wireguard_peers wp
         JOIN servers s ON s.id = wp.server_id
         WHERE wp.status = 'active'
           AND wp.public_key IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let mut server_names_taken: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for row in rows.flatten() {
        let host = normalize_mesh_hostname(&row.0);
        server_names_taken.insert(host.clone());
        out.push((host, row.1));
    }

    // Drop the prepared statement before opening another one — sqlite
    // doesn't allow two concurrent prepared statements on the same
    // connection in rusqlite's binding.
    drop(stmt);

    // Service-DNS overlay (Etap 3.3): each row in `service_dns` maps a
    // logical name → host server. Inject `<name>.mesh` → server's mesh
    // IP so consumer stacks can `postgres://db.mesh:5432/...` instead
    // of hard-coding the per-node hostname.
    //
    // We only emit a row when:
    //   - the target server has an active wireguard_peers row (i.e. the
    //     IP we'd inject is the same one its server-mesh hostname is
    //     already using)
    //   - the logical name doesn't collide with an existing
    //     <server>.mesh hostname (the API layer refuses these at
    //     INSERT time, but defence-in-depth here costs nothing)
    let mut stmt_dns = match db.prepare(
        "SELECT sd.name, wp.assigned_ip \
         FROM service_dns sd \
         JOIN wireguard_peers wp ON wp.server_id = sd.server_id \
         WHERE wp.status = 'active'",
    ) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let dns_rows = match stmt_dns.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for row in dns_rows.flatten() {
        let host = format!("{}.mesh", row.0);
        if server_names_taken.contains(&host) {
            continue;
        }
        out.push((host, row.1));
    }
    out
}

/// Redeploy every locally-managed compose stack to pick up a fresh
/// `extra_hosts` map. Used by the service-DNS CRUD endpoints to make
/// changes visible to running containers — extra_hosts is only read
/// at container create-time, so a `docker compose up -d` against the
/// existing spec is what actually plumbs the new entry through.
///
/// Async background task. Per-stack failures are logged at WARN but
/// don't abort the rest of the pass — the operator's intent is "best
/// effort, get most stacks pointing at the new IP" and we shouldn't
/// hold up the API response or strand the remaining stacks just
/// because one of them had an image pull glitch.
pub fn spawn_redeploy_all_compose(state: crate::state::SharedState) {
    tokio::spawn(async move {
        // Snapshot the candidate list under the DB lock, then release
        // before any docker work. Each redeploy can take seconds, so
        // holding the lock across the loop would block every other
        // request.
        let candidates: Vec<(String, String, String)> = {
            let db = match state.db.lock() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("redeploy_all_compose: DB lock: {e}");
                    return;
                }
            };
            let mut stmt = match db.prepare(
                "SELECT id, name, compose_content \
                 FROM services \
                 WHERE service_type = 'compose' \
                   AND compose_content IS NOT NULL \
                   AND compose_content <> '' \
                   AND status = 'running' \
                   AND owner_server_id IS NULL",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("redeploy_all_compose: prepare: {e}");
                    return;
                }
            };
            let rows = match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("redeploy_all_compose: query: {e}");
                    return;
                }
            };
            rows.filter_map(|r| r.ok()).collect()
        };

        if candidates.is_empty() {
            tracing::debug!("redeploy_all_compose: no running stacks to refresh");
            return;
        }
        tracing::info!(
            "redeploy_all_compose: refreshing {} stack(s) after mesh-hosts change",
            candidates.len()
        );

        for (id, name, yaml) in candidates {
            // Same auth-map pickup as `api::compose::deploy` — private
            // registry credentials still apply to a redeploy.
            let auth = state
                .db
                .lock()
                .ok()
                .and_then(|db| crate::docker::auth::auth_map_for_service(&db, &id).ok())
                .filter(|m| !m.is_empty());
            match crate::docker::deploy_service_stack(&state, &id, &name, &yaml, auth).await {
                Ok(_) => tracing::info!("redeploy_all_compose: {name} ok"),
                Err(e) => tracing::warn!("redeploy_all_compose: {name} failed: {e}"),
            }
        }
    });
}

fn normalize_mesh_hostname(name: &str) -> String {
    let mut s = String::with_capacity(name.len() + 5);
    let mut prev_dash = true; // suppress leading dashes
    for c in name.chars() {
        let ch = c.to_ascii_lowercase();
        let allowed = ch.is_ascii_alphanumeric() || ch == '.' || ch == '-';
        if allowed {
            s.push(ch);
            prev_dash = ch == '-' || ch == '.';
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    while s.ends_with('-') || s.ends_with('.') {
        s.pop();
    }
    if s.is_empty() {
        s.push_str("peer");
    }
    s.push_str(".mesh");
    s
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
    use super::{
        env_json_to_env_content, inject_env_file_into_services,
        inject_mesh_extra_hosts_into_services, normalize_mesh_hostname,
    };
    use crate::crypto::encrypt_env_json;

    fn hosts() -> Vec<(String, String)> {
        vec![
            ("vps1.mesh".into(), "10.42.0.1".into()),
            ("vps2.mesh".into(), "10.42.0.2".into()),
        ]
    }

    #[test]
    fn mesh_hosts_inject_into_service_without_extra_hosts() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &hosts());
        assert!(out.contains("extra_hosts:"));
        assert!(out.contains(r#"- "vps1.mesh:10.42.0.1""#));
        assert!(out.contains(r#"- "vps2.mesh:10.42.0.2""#));
        assert_eq!(out.matches("extra_hosts:").count(), 1);
    }

    #[test]
    fn mesh_hosts_skip_service_with_user_extra_hosts() {
        // The operator put their own extra_hosts in — don't clobber it.
        // They miss out on mesh-DNS for this service; that's the trade.
        let yaml =
            "services:\n  backend:\n    image: foo:bar\n    extra_hosts:\n      - \"db:1.2.3.4\"\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &hosts());
        assert!(out.contains("db:1.2.3.4"));
        assert_eq!(out.matches("extra_hosts:").count(), 1);
        assert!(!out.contains("vps1.mesh"));
    }

    #[test]
    fn mesh_hosts_inject_into_every_service_in_multi_service_compose() {
        let yaml = "services:\n  backend:\n    image: foo:bar\n  worker:\n    image: baz:qux\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &hosts());
        assert_eq!(out.matches("extra_hosts:").count(), 2);
        assert_eq!(out.matches("vps1.mesh:10.42.0.1").count(), 2);
    }

    #[test]
    fn mesh_hosts_noop_on_empty_list() {
        // When mesh is disabled or has no active peers, the helper hands
        // us an empty slice and the YAML must round-trip unchanged.
        let yaml = "services:\n  backend:\n    image: foo:bar\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &[]);
        assert_eq!(out, yaml);
    }

    #[test]
    fn mesh_hosts_noop_without_services_block() {
        let yaml = "version: '3'\nnetworks:\n  pier-net:\n    external: true\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &hosts());
        assert!(!out.contains("extra_hosts:"));
    }

    #[test]
    fn mesh_hosts_does_not_leak_into_top_level_networks() {
        let yaml = "services:\n  backend:\n    image: foo:bar\nnetworks:\n  pier-net:\n    external: true\n";
        let out = inject_mesh_extra_hosts_into_services(yaml, &hosts());
        let extra_pos = out.find("extra_hosts:").expect("must inject");
        let networks_pos = out
            .find("\nnetworks:")
            .expect("must preserve top-level networks");
        assert!(extra_pos < networks_pos);
        assert_eq!(out.matches("extra_hosts:").count(), 1);
    }

    #[test]
    fn normalize_lowercases_and_replaces_spaces() {
        assert_eq!(normalize_mesh_hostname("VPS 1 Master"), "vps-1-master.mesh");
    }

    #[test]
    fn normalize_keeps_dots_and_dashes() {
        assert_eq!(
            normalize_mesh_hostname("vps1.master-eu"),
            "vps1.master-eu.mesh"
        );
    }

    #[test]
    fn normalize_collapses_punctuation_runs() {
        // `__` and `&` both unmap to `-`; consecutive disallowed chars
        // collapse so we don't end up with `vps---1.mesh`.
        assert_eq!(normalize_mesh_hostname("vps__&1"), "vps-1.mesh");
    }

    #[test]
    fn normalize_strips_trailing_punctuation() {
        assert_eq!(normalize_mesh_hostname("vps1!!!"), "vps1.mesh");
    }

    #[test]
    fn normalize_falls_back_when_input_is_all_garbage() {
        // All disallowed chars → empty → fall back to "peer.mesh" so
        // we never emit a malformed `extra_hosts: - ":10.42.0.1"`.
        assert_eq!(normalize_mesh_hostname("!!!"), "peer.mesh");
    }

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
