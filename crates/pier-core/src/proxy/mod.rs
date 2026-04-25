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

pub const DEFAULT_TRAEFIK_VERSION: &str = "v3.3";
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

    // Pull image if not present
    let image = traefik_image(version);
    pull_image_if_needed(docker, &image, None).await?;

    // Remove old container if exists
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

    // Bridge mode + pier-net: Traefik accesses services via Docker DNS (container names).
    // Port bindings: 80, 443, + all active TCP public ports.
    // When new TCP port is added, Traefik is recreated with updated port bindings.
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
    // Add TCP port bindings from traefik.yml entryPoints (e.g., 5432 for PostgreSQL)
    let tcp_ports = config::read_tcp_ports_from_config(data_dir);
    for port in &tcp_ports {
        port_bindings.insert(
            format!("{port}/tcp"),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(port.to_string()),
            }]),
        );
    }
    if !tcp_ports.is_empty() {
        tracing::info!("Traefik TCP port bindings: {:?}", tcp_ports);
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

/// Reconcile Traefik configuration with the current `port_allocations` rows
/// for a given service. Idempotent and safe to call after any change that
/// affects `is_public` / `public_port` for the service:
///
/// - `set_port_public` toggle from the UI
/// - `update_ports_from_compose` after a redeploy
/// - manual DB edits
///
/// Behavior:
/// 1. Removes all stale `tcp-{service_id}-*.yml` dynamic configs whose port is
///    no longer marked `is_public=1`.
/// 2. Writes a fresh `tcp-{service_id}-{public_port}.yml` for each public port.
/// 3. Regenerates the static config with the union of every public TCP port in
///    the database (across all services) and recreates the Traefik container
///    with matching host port bindings.
///
/// On a service with zero public ports, all per-service TCP files are deleted.
pub async fn sync_tcp_routes_for_service(
    state: &crate::state::AppState,
    service_id: &str,
) -> Result<()> {
    // Snapshot all DB state under one lock.
    struct Snapshot {
        public_ports: Vec<(u16, u16)>, // (public_port, container_port)
        service_name: String,
        container_name: Option<String>,
        all_tcp_ports: Vec<u16>,
        acme_email: String,
        dashboard: bool,
        traefik_version: String,
    }

    let snap = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let mut stmt = db.prepare(
            "SELECT public_port, container_port FROM port_allocations \
             WHERE service_id = ?1 AND is_public = 1 AND public_port IS NOT NULL \
             ORDER BY port_name",
        )?;
        let public_ports: Vec<(u16, u16)> = stmt
            .query_map([service_id], |row| {
                Ok((row.get::<_, i64>(0)? as u16, row.get::<_, i64>(1)? as u16))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let (service_name, container_name): (String, Option<String>) = db
            .query_row(
                "SELECT name, container_id FROM services WHERE id = ?1",
                [service_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| anyhow::anyhow!("Service {service_id} not found"))?;

        let mut s2 = db.prepare(
            "SELECT DISTINCT public_port FROM port_allocations \
             WHERE is_public = 1 AND public_port IS NOT NULL",
        )?;
        let all_tcp_ports: Vec<u16> = s2
            .query_map([], |row| row.get::<_, i64>(0).map(|p| p as u16))?
            .filter_map(|r| r.ok())
            .collect();

        let acme_email = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.acme_email'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "admin@pier.local".to_string());
        let dashboard = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default()
            == "true";
        let traefik_version = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.traefik_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .filter(|v: &String| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_TRAEFIK_VERSION.to_string());

        Snapshot {
            public_ports,
            service_name,
            container_name,
            all_tcp_ports,
            acme_email,
            dashboard,
            traefik_version,
        }
    };

    // Resolve upstream Docker DNS name. Compose services with explicit
    // `container_name:` (e.g. `myhome-backend`) need the actual container
    // name; auto-named services fall back to `pier-{slug}`.
    let upstream_host = snap
        .container_name
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "pier-{}",
                snap.service_name.to_lowercase().replace(' ', "-")
            )
        });

    // Wipe stale per-service TCP files whose port is no longer public, then
    // re-emit a file per current public port.
    let want_ports: std::collections::HashSet<u16> =
        snap.public_ports.iter().map(|(p, _)| *p).collect();
    let dynamic_dir = state.config.data_dir.join("traefik").join("dynamic");
    if let Ok(entries) = std::fs::read_dir(&dynamic_dir) {
        let prefix = format!("tcp-{service_id}-");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(&prefix) && name_str.ends_with(".yml") {
                let port_str = &name_str[prefix.len()..name_str.len() - ".yml".len()];
                if let Ok(p) = port_str.parse::<u16>() {
                    if !want_ports.contains(&p) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
    // Drop the legacy single-file tcp-{service_id}.yml — superseded by
    // the per-port files even if the service has just one public port.
    let legacy = dynamic_dir.join(format!("tcp-{service_id}.yml"));
    if legacy.exists() {
        let _ = std::fs::remove_file(&legacy);
    }

    for (public_port, container_port) in &snap.public_ports {
        let upstreams = vec![format!("{upstream_host}:{container_port}")];
        config::write_tcp_route_lb(&state.config.data_dir, service_id, *public_port, &upstreams)?;
    }

    config::regenerate_static_config_with_tcp(
        &state.config.data_dir,
        &snap.acme_email,
        snap.dashboard,
        &snap.all_tcp_ports,
    )?;

    deploy_traefik(
        &state.docker,
        &state.config.data_dir,
        &snap.acme_email,
        snap.dashboard,
        &snap.traefik_version,
    )
    .await?;

    tracing::info!(
        "Synced TCP routes for {service_id}: ports {:?} → {upstream_host}",
        snap.public_ports
    );
    Ok(())
}
