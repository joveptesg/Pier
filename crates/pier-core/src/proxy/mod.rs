pub mod config;
pub mod ssl_monitor;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use bollard::auth::DockerCredentials;
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::Docker;

pub const DEFAULT_TRAEFIK_VERSION: &str = "v3.7.1";
const TRAEFIK_CONTAINER: &str = "pier-traefik";
const PIER_NETWORK: &str = "pier-net";

/// Serializes `deploy_traefik` / `stop_traefik` across callers. Without this,
/// two concurrent toggles of "Make publicly available" race on stop→remove→create
/// and the loser fails with Docker 409 "container name is already in use".
static TRAEFIK_DEPLOY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Compose the full `traefik:<version>` image tag.
pub fn traefik_image(version: &str) -> String {
    format!("traefik:{version}")
}

/// Resolve the ACME contact email for Let's Encrypt registration.
///
/// Order: explicit `proxy.acme_email` setting → first admin user's email →
/// hardcoded fallback. Always read fresh — never cache. The admin-email
/// fallback matters on fresh installs where Traefik auto-starts before the
/// operator has run `/setup`; reading dynamically lets a later setup propagate
/// without restarting Pier.
pub fn read_acme_email(db: &rusqlite::Connection) -> String {
    db.query_row(
        "SELECT value FROM settings WHERE key = 'proxy.acme_email'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .filter(|v| !v.is_empty())
    .unwrap_or_else(|| {
        db.query_row(
            "SELECT email FROM users WHERE role = 'admin' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "admin@pier.local".to_string())
    })
}

/// Deploy and start the Traefik reverse proxy container.
pub async fn deploy_traefik(
    docker: &Docker,
    data_dir: &Path,
    acme_email: &str,
    dashboard: bool,
    version: &str,
) -> Result<()> {
    // Serialize concurrent redeploys (e.g. two near-simultaneous public-port
    // toggles) so stop→remove→create isn't racing against itself.
    let _guard = TRAEFIK_DEPLOY_LOCK.lock().await;

    // Write Traefik config files
    config::write_static_config(data_dir, acme_email, dashboard)?;

    // Detect the data volume/path for sharing configs with Traefik.
    // Supports: env var override, Docker named volume (containerized), or host path (native).
    let data_volume = detect_data_volume(docker, data_dir).await?;
    tracing::info!("Using data volume: {data_volume}");

    // Ensure pier-net network exists
    ensure_network(docker).await?;

    // Pull image if not present (resilient: retries + Docker Hub mirror fallback)
    let image = traefik_image(version);
    pull_traefik_image(docker, version).await?;

    // Remove old container if exists. Log at debug — errors are usually
    // just "no such container" on first deploy, but capturing them helps
    // diagnose port-binding races on later updates.
    if let Err(e) = docker.stop_container(TRAEFIK_CONTAINER, None).await {
        tracing::debug!("Stop old Traefik (ignored): {e}");
    }
    if let Err(e) = docker
        .remove_container(
            TRAEFIK_CONTAINER,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
    {
        tracing::debug!("Remove old Traefik (ignored): {e}");
    }

    // Bridge mode + pier-net: Traefik accesses services via Docker DNS (container names).
    // Port bindings: 80, 443 only (+ 8080 for dashboard). Raw TCP exposure
    // for user services is done via direct Docker `-p` on the service
    // container itself, so Traefik never needs to restart when an operator
    // toggles "Make publicly available" — this is the whole point of the
    // Coolify-style architecture: HTTP routes hot-reload via file provider,
    // TCP doesn't touch Traefik at all.
    let mut port_bindings = std::collections::HashMap::new();
    port_bindings.insert(
        "80/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("80".to_string()),
        }]),
    );
    port_bindings.insert(
        "443/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some("443".to_string()),
        }]),
    );
    if dashboard {
        port_bindings.insert(
            "8080/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("8080".to_string()),
            }]),
        );
    }

    let host_config = HostConfig {
        port_bindings: Some(port_bindings),
        binds: Some(vec![format!("{data_volume}:/data")]),
        network_mode: Some(PIER_NETWORK.to_string()),
        restart_policy: Some(bollard::models::RestartPolicy {
            name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
            ..Default::default()
        }),
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(image.clone()),
        cmd: Some(vec!["--configFile=/data/traefik/traefik.yml".to_string()]),
        hostname: Some(TRAEFIK_CONTAINER.to_string()),
        host_config: Some(host_config),
        labels: Some(HashMap::from([
            ("pier.managed".to_string(), "true".to_string()),
            ("pier.role".to_string(), "proxy".to_string()),
        ])),
        ..Default::default()
    };

    docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(TRAEFIK_CONTAINER.to_string()),
                ..Default::default()
            }),
            config,
        )
        .await?;

    docker
        .start_container(TRAEFIK_CONTAINER, None::<StartContainerOptions>)
        .await?;

    // Health-check: Docker's start_container returns OK as soon as the container
    // is launched, not when the process inside is stable. Some Traefik versions
    // (e.g. 3.7 on certain configs) exit shortly after start without writing a
    // fatal log line. Wait a few seconds, then verify the container is still
    // running. If not — surface the last log lines so the caller can diagnose
    // (and rollback if applicable).
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let info = docker.inspect_container(TRAEFIK_CONTAINER, None).await?;
    let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
    if !running {
        let exit_code = info.state.as_ref().and_then(|s| s.exit_code).unwrap_or(-1);
        let logs = crate::docker::logs::get_logs(docker, TRAEFIK_CONTAINER, 50, false)
            .await
            .unwrap_or_else(|_| Vec::new())
            .join("\n");
        return Err(anyhow::anyhow!(
            "Traefik container exited shortly after start (exit_code={exit_code}). Last logs:\n{logs}"
        ));
    }

    tracing::info!("Traefik proxy started on ports 80/443");
    Ok(())
}

/// Restart the Traefik container (for static config changes like new entryPoints).
#[allow(dead_code)]
pub async fn restart_traefik(docker: &Docker) -> Result<()> {
    docker.restart_container(TRAEFIK_CONTAINER, None).await?;
    tracing::info!("Traefik restarted for config update");
    Ok(())
}

/// Stop and remove the Traefik container.
pub async fn stop_traefik(docker: &Docker) -> Result<()> {
    let _guard = TRAEFIK_DEPLOY_LOCK.lock().await;
    let _ = docker.stop_container(TRAEFIK_CONTAINER, None).await;
    let _ = docker
        .remove_container(
            TRAEFIK_CONTAINER,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    tracing::info!("Traefik proxy stopped");
    Ok(())
}

/// Get Traefik container status.
pub async fn traefik_status(docker: &Docker) -> Result<TraefikStatus> {
    match docker.inspect_container(TRAEFIK_CONTAINER, None).await {
        Ok(info) => {
            let state = info.state.as_ref();
            let running = state.and_then(|s| s.running).unwrap_or(false);
            let status = state
                .and_then(|s| s.status.as_ref())
                .map(|s| format!("{s:?}"))
                .unwrap_or_else(|| "unknown".to_string());
            Ok(TraefikStatus {
                installed: true,
                running,
                status,
                image: info
                    .config
                    .as_ref()
                    .and_then(|c| c.image.clone())
                    .unwrap_or_default(),
            })
        }
        Err(_) => Ok(TraefikStatus {
            installed: false,
            running: false,
            status: "not installed".to_string(),
            image: String::new(),
        }),
    }
}

#[derive(serde::Serialize)]
pub struct TraefikStatus {
    pub installed: bool,
    pub running: bool,
    pub status: String,
    pub image: String,
}

/// Ensure the pier-net Docker network exists.
async fn ensure_network(docker: &Docker) -> Result<()> {
    match docker.inspect_network(PIER_NETWORK, None).await {
        Ok(_) => {}
        Err(_) => {
            docker
                .create_network(NetworkCreateRequest {
                    name: PIER_NETWORK.to_string(),
                    driver: Some("bridge".to_string()),
                    ..Default::default()
                })
                .await?;
            tracing::info!("Created Docker network: {PIER_NETWORK}");
        }
    }
    Ok(())
}

/// Pull an image if not already present. `creds` plumbs registry auth for
/// private images; callers resolve it via `docker::auth::credentials_for`.
/// Traefik's own image is public, so the current caller passes `None`.
#[allow(dead_code)]
pub async fn pull_image_if_needed(
    docker: &Docker,
    image: &str,
    creds: Option<DockerCredentials>,
) -> Result<()> {
    use futures_util::StreamExt;

    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }

    tracing::info!("Pulling {image}...");
    let opts = CreateImageOptions {
        from_image: Some(image.to_string()),
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(opts), None, creds);
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            return Err(anyhow::anyhow!("Failed to pull {image}: {e}"));
        }
    }

    tracing::info!("Pulled {image}");
    Ok(())
}

/// Public Docker Hub `library/*` mirrors used as fallback for the Traefik
/// image when Docker Hub rate-limits the host. Both serve the exact same
/// layers as `docker.io/library/*` (read-only pull-through caches operated
/// by Google Cloud and AWS respectively) — no modification, no auth needed.
const TRAEFIK_MIRRORS: &[&str] = &["mirror.gcr.io/library", "public.ecr.aws/docker/library"];

fn is_rate_limit_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("rate limit") || m.contains("rate-limit") || m.contains("toomanyrequests")
}

/// Pull `traefik:<version>` resiliently.
///
/// Strategy:
///   1. Short-circuit if the image is already cached locally.
///   2. Try Docker Hub up to 3 times with backoff (5s → 15s → 45s).
///   3. On detected rate-limit, skip remaining Docker Hub retries and jump
///      to mirrors immediately.
///   4. Try public mirrors (`mirror.gcr.io/library`, `public.ecr.aws/docker/library`).
///   5. On mirror success, `docker tag <mirror>/traefik:<v>` back to
///      `traefik:<v>` so downstream `inspect_image` / container spec keeps working.
pub async fn pull_traefik_image(docker: &Docker, version: &str) -> Result<()> {
    let canonical = traefik_image(version);

    if docker.inspect_image(&canonical).await.is_ok() {
        return Ok(());
    }

    let backoffs_s = [5u64, 15, 45];
    let mut last_err: Option<String> = None;
    let mut rate_limited = false;

    for (i, delay) in backoffs_s.iter().enumerate() {
        tracing::info!("Pulling {canonical} (attempt {}/3)...", i + 1);
        match pull_image_attempt(docker, &canonical, None).await {
            Ok(()) => {
                tracing::info!("Pulled {canonical}");
                return Ok(());
            }
            Err(e) => {
                let msg = e.to_string();
                if is_rate_limit_error(&msg) {
                    tracing::warn!(
                        "Docker Hub rate-limited pulling {canonical}; switching to public mirrors"
                    );
                    last_err = Some(msg);
                    rate_limited = true;
                    break;
                }
                tracing::warn!("Pull {canonical} attempt {} failed: {msg}", i + 1);
                last_err = Some(msg);
                if i + 1 < backoffs_s.len() {
                    tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
                }
            }
        }
    }

    let traefik_ref = format!("traefik:{version}");
    for mirror in TRAEFIK_MIRRORS {
        let mirror_image = format!("{mirror}/{traefik_ref}");
        tracing::info!("Trying mirror: pulling {mirror_image}...");
        match pull_image_attempt(docker, &mirror_image, None).await {
            Ok(()) => {
                let tag_opts = bollard::query_parameters::TagImageOptions {
                    repo: Some("traefik".to_string()),
                    tag: Some(version.to_string()),
                };
                docker
                    .tag_image(&mirror_image, Some(tag_opts))
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Pulled {mirror_image} but failed to retag as {canonical}: {e}"
                        )
                    })?;
                tracing::info!("Pulled {mirror_image} and tagged as {canonical}");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("Mirror {mirror_image} failed: {e}");
                last_err = Some(e.to_string());
            }
        }
    }

    let reason = if rate_limited {
        "Docker Hub rate-limited and all public mirrors failed"
    } else {
        "Docker Hub and all public mirrors failed"
    };
    Err(anyhow::anyhow!(
        "Failed to pull {canonical}: {reason}. Last error: {}",
        last_err.as_deref().unwrap_or("(none)")
    ))
}

/// One-shot Docker pull without the `inspect_image` short-circuit.
/// Used by `pull_traefik_image` for individual attempts so retry/fallback
/// logic stays in one place.
async fn pull_image_attempt(
    docker: &Docker,
    image: &str,
    creds: Option<DockerCredentials>,
) -> Result<()> {
    use futures_util::StreamExt;
    let opts = CreateImageOptions {
        from_image: Some(image.to_string()),
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(opts), None, creds);
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            return Err(anyhow::anyhow!("{e}"));
        }
    }
    Ok(())
}

const PIER_CONTAINER: &str = "pier";
const PIER_DATA_MOUNT: &str = "/app/data";

/// Detect the data volume or host path for sharing with Traefik.
///
/// Priority:
/// 1. `PIER_DATA_VOLUME` env var (explicit override)
/// 2. Docker named volume (when Pier runs inside a container)
/// 3. Absolute host path from `data_dir` (native installation)
async fn detect_data_volume(docker: &Docker, data_dir: &Path) -> Result<String> {
    // 1. Explicit env var (highest priority)
    if let Ok(vol) = std::env::var("PIER_DATA_VOLUME") {
        if !vol.is_empty() {
            return Ok(vol);
        }
    }

    // 2. Try Docker volume detection (when Pier runs in container)
    if let Ok(info) = docker.inspect_container(PIER_CONTAINER, None).await {
        if let Some(mounts) = info.mounts {
            for mount in &mounts {
                let dest = mount.destination.as_deref().unwrap_or_default();
                if dest == PIER_DATA_MOUNT {
                    if let Some(name) = &mount.name {
                        return Ok(name.clone());
                    }
                }
            }
        }
    }

    // 3. Native mode: use absolute host path as bind-mount source for Traefik
    let abs = std::fs::canonicalize(data_dir)
        .map_err(|e| anyhow::anyhow!("Cannot resolve data_dir '{}': {e}", data_dir.display()))?;
    tracing::info!("Native mode detected — using host path for Traefik bind-mount");
    Ok(abs.to_string_lossy().to_string())
}

/// One-shot migration: for every service that previously had a Traefik TCP
/// route (`port_allocations.is_public=1`), force-recreate its compose stack so
/// the public port is now exposed via a direct Docker `-p` binding on the
/// service container. Then purge all legacy `tcp-*.yml` files from
/// `traefik/dynamic/` so Traefik no longer carries dead routes.
///
/// Idempotent: if no public ports exist, returns 0 without touching anything.
/// Safe on every startup — running it twice does no harm because:
///   - The compose YAML already reflects the new public binding after the
///     first migration (the catalog rebuild path always emits the same form).
///   - `docker compose up -d` is a no-op when nothing changed.
///   - Legacy `tcp-*.yml` deletion just confirms zero matches.
///
/// Returns `(deferred_services, removed_tcp_files)` where `deferred_services`
/// counts services that still carry `is_public=1` in DB but whose public
/// binding will only re-materialize on the next deliberate redeploy.
///
/// **Critical:** this function does NOT recreate any service container. An
/// earlier version did (`rebuild_and_redeploy_for_port_toggle` in a loop)
/// and produced a production-wide outage when the regenerated catalog YAML
/// didn't match the actual container_name on real deployments (e.g. servers
/// using `pier-{svc}-srv0` naming): `docker compose up -d` detached the
/// live container from every Docker network, breaking DNS for every
/// consumer of that service. We now only purge dead Traefik TCP route
/// files and rely on `reconcile_pier_managed_networks` (also on every
/// startup) to repair any container that lost its networks.
pub async fn migrate_public_ports_to_direct_binding(
    state: &crate::state::AppState,
) -> Result<(usize, usize)> {
    let removed = config::purge_legacy_tcp_route_files(&state.config.data_dir);
    if removed > 0 {
        tracing::info!("Migration: removed {removed} legacy tcp-*.yml file(s)");
    }

    let service_ids: Vec<String> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT DISTINCT service_id FROM port_allocations \
             WHERE is_public = 1 AND public_port IS NOT NULL",
        )?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if !service_ids.is_empty() {
        tracing::info!(
            "Migration: {} service(s) carry is_public=1 in DB. Their public Docker port binding will be (re-)applied on the next operator-triggered redeploy of each service (UI Toggle off→on, or Redeploy). Affected: {:?}",
            service_ids.len(),
            service_ids
        );
    }

    Ok((service_ids.len(), removed))
}

/// Reconcile Docker network attachments for every pier-managed container.
///
/// **Why this exists:** a buggy earlier version of `migrate_public_ports_to_direct_binding`
/// recreated service stacks via `docker compose up -d` against a regenerated
/// YAML that didn't match real-world container names. That detached running
/// containers from `pier-net` and any per-project network they were on,
/// breaking Docker DNS for every consumer (symptom:
/// `getaddrinfo EAI_AGAIN pier-<service>`).
///
/// **What it does:** locates every container labelled `pier.service.id` (the
/// label is emitted by `catalog::build_compose_yaml_scaled`, so every catalog
/// service has it regardless of its container_name), computes the set of
/// networks the container should be on (always `pier-net`, plus the
/// per-project network from `services.network_id` if set), and reattaches
/// missing networks. Idempotent — once a container is on its expected
/// networks, calling this is a no-op (Docker returns "already in network",
/// which we swallow).
///
/// Container-name agnostic on purpose: production servers use names like
/// `pier-postgresql-srv0`, `pier-redis`, `foooh-back`, etc. — we never
/// hardcode any of them.
///
/// Returns `(attached_count, scanned_count)` for the summary log line.
pub async fn reconcile_pier_managed_networks(
    state: &crate::state::AppState,
) -> Result<(usize, usize)> {
    use bollard::models::{EndpointSettings, NetworkConnectRequest};
    use bollard::query_parameters::ListContainersOptions;

    let containers = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: false,
            ..Default::default()
        }))
        .await?;

    // Build (service_id -> project_network_name) lookup once.
    let project_networks: std::collections::HashMap<String, String> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT s.id, n.name FROM services s \
             JOIN networks n ON n.id = s.network_id",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        rows.into_iter().collect()
    };

    let mut attached = 0usize;
    let mut scanned = 0usize;

    for container in &containers {
        // Extract pier.service.id label; skip non-pier containers (Traefik,
        // user-deployed-outside-of-pier, etc.).
        let labels = match container.labels.as_ref() {
            Some(l) => l,
            None => continue,
        };
        let service_id = match labels.get("pier.service.id") {
            Some(s) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        let container_id = match container.id.as_ref() {
            Some(id) => id.clone(),
            None => continue,
        };
        let display_name = container
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| container_id.chars().take(12).collect());

        scanned += 1;

        // Current networks: list_containers gives a summary; we need
        // inspect_container to see the full Networks map.
        let info = match state.docker.inspect_container(&container_id, None).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("Reconcile: inspect {display_name} failed: {e}");
                continue;
            }
        };
        let current: std::collections::HashSet<String> = info
            .network_settings
            .as_ref()
            .and_then(|ns| ns.networks.as_ref())
            .map(|nets| nets.keys().cloned().collect())
            .unwrap_or_default();

        // Expected networks: pier-net (always) + project network if any.
        let mut expected: Vec<String> = vec![PIER_NETWORK.to_string()];
        if let Some(proj) = project_networks.get(&service_id) {
            if proj != PIER_NETWORK && !proj.is_empty() {
                expected.push(proj.clone());
            }
        }

        for net in expected {
            if current.contains(&net) {
                continue;
            }
            let req = NetworkConnectRequest {
                container: container_id.clone(),
                endpoint_config: Some(EndpointSettings::default()),
            };
            match state.docker.connect_network(&net, req).await {
                Ok(()) => {
                    attached += 1;
                    tracing::info!(
                        "Reconcile: attached {display_name} (service {service_id}) to network {net}"
                    );
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Common benign errors when the container is already
                    // attached (race with another reconciler / docker
                    // compose). Swallow them — the goal state is already met.
                    if msg.contains("already exists in network")
                        || msg.contains("already attached to network")
                    {
                        continue;
                    }
                    tracing::warn!("Reconcile: failed to attach {display_name} to {net}: {msg}");
                }
            }
        }
    }

    Ok((attached, scanned))
}
