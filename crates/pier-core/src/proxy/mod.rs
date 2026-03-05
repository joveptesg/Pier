pub mod config;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest, PortBinding};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::Docker;

const TRAEFIK_IMAGE: &str = "traefik:v3.3";
const TRAEFIK_CONTAINER: &str = "pier-traefik";
const PIER_NETWORK: &str = "pier-net";

/// Deploy and start the Traefik reverse proxy container.
pub async fn deploy_traefik(
    docker: &Docker,
    data_dir: &Path,
    acme_email: &str,
    dashboard: bool,
) -> Result<()> {
    // Write Traefik config files
    config::write_static_config(data_dir, acme_email, dashboard)?;

    // Detect the Docker named volume that holds Pier's data directory.
    // We inspect our own container to find which volume is mounted at /app/data.
    let data_volume = detect_data_volume(docker).await?;
    tracing::info!("Using data volume: {data_volume}");

    // Ensure pier-net network exists
    ensure_network(docker).await?;

    // Pull image if not present
    pull_image_if_needed(docker).await?;

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

    // Port bindings: 80 and 443
    let mut port_bindings = HashMap::new();
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

    // Use the same named volume as Pier so Traefik can read configs written by Pier.
    // Bind-mount paths don't work because Pier runs in a container — the host doesn't
    // have /app/data. Named volumes are shared correctly between sibling containers.
    let host_config = HostConfig {
        port_bindings: Some(port_bindings),
        binds: Some(vec![
            format!("{data_volume}:/data"),
        ]),
        network_mode: Some(PIER_NETWORK.to_string()),
        restart_policy: Some(bollard::models::RestartPolicy {
            name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
            ..Default::default()
        }),
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(TRAEFIK_IMAGE.to_string()),
        cmd: Some(vec![
            "--configFile=/data/traefik/traefik.yml".to_string(),
        ]),
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

/// Stop and remove the Traefik container.
pub async fn stop_traefik(docker: &Docker) -> Result<()> {
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

/// Pull Traefik image if not already present.
async fn pull_image_if_needed(docker: &Docker) -> Result<()> {
    use futures_util::StreamExt;

    if docker.inspect_image(TRAEFIK_IMAGE).await.is_ok() {
        return Ok(());
    }

    tracing::info!("Pulling {TRAEFIK_IMAGE}...");
    let opts = CreateImageOptions {
        from_image: Some(TRAEFIK_IMAGE.to_string()),
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            return Err(anyhow::anyhow!("Failed to pull {TRAEFIK_IMAGE}: {e}"));
        }
    }

    tracing::info!("Pulled {TRAEFIK_IMAGE}");
    Ok(())
}

const PIER_CONTAINER: &str = "pier";
const PIER_DATA_MOUNT: &str = "/app/data";

/// Detect the Docker named volume that holds Pier's data directory.
/// Inspects the Pier container to find which volume is mounted at /app/data.
/// Falls back to env var PIER_DATA_VOLUME if container inspection fails.
async fn detect_data_volume(docker: &Docker) -> Result<String> {
    // Check env var first
    if let Ok(vol) = std::env::var("PIER_DATA_VOLUME") {
        if !vol.is_empty() {
            return Ok(vol);
        }
    }

    // Inspect own container
    let info = docker.inspect_container(PIER_CONTAINER, None).await?;
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

    Err(anyhow::anyhow!(
        "Could not detect data volume. Set PIER_DATA_VOLUME env var."
    ))
}
