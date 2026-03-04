use anyhow::Result;
use bollard::query_parameters::{
    ListContainersOptions, RemoveContainerOptions, RestartContainerOptions,
    StopContainerOptions,
};
use bollard::Docker;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
    pub ports: Vec<PortMapping>,
    pub created: i64,
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortMapping {
    pub private_port: u16,
    pub public_port: Option<u16>,
    pub port_type: String,
}

/// List all containers.
pub async fn list_containers(docker: &Docker, all: bool) -> Result<Vec<ContainerInfo>> {
    let opts = ListContainersOptions {
        all,
        ..Default::default()
    };

    let containers = docker.list_containers(Some(opts)).await?;

    let result = containers
        .into_iter()
        .map(|c| {
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();

            let ports = c
                .ports
                .unwrap_or_default()
                .into_iter()
                .map(|p| PortMapping {
                    private_port: p.private_port as u16,
                    public_port: p.public_port.map(|pp| pp as u16),
                    port_type: p.typ.map(|t| format!("{t:?}").to_lowercase()).unwrap_or_else(|| "tcp".into()),
                })
                .collect();

            let state = c
                .state
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_else(|| "unknown".into());

            ContainerInfo {
                id: c.id.unwrap_or_default(),
                name,
                image: c.image.unwrap_or_default(),
                status: c.status.unwrap_or_default(),
                state,
                ports,
                created: c.created.unwrap_or(0),
                labels: c.labels.unwrap_or_default(),
            }
        })
        .collect();

    Ok(result)
}

/// Get detailed info about a single container.
pub async fn inspect_container(
    docker: &Docker,
    id: &str,
) -> Result<bollard::models::ContainerInspectResponse> {
    Ok(docker.inspect_container(id, None).await?)
}

/// Start a container.
pub async fn start_container(docker: &Docker, id: &str) -> Result<()> {
    docker.start_container(id, None).await?;
    Ok(())
}

/// Stop a container with a 10-second timeout.
pub async fn stop_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .stop_container(
            id,
            Some(StopContainerOptions {
                t: Some(10),
                signal: None,
            }),
        )
        .await?;
    Ok(())
}

/// Restart a container.
pub async fn restart_container(docker: &Docker, id: &str) -> Result<()> {
    docker
        .restart_container(
            id,
            Some(RestartContainerOptions {
                t: Some(10),
                signal: None,
            }),
        )
        .await?;
    Ok(())
}

/// Remove a container.
pub async fn remove_container(docker: &Docker, id: &str, force: bool) -> Result<()> {
    docker
        .remove_container(
            id,
            Some(RemoveContainerOptions {
                force,
                v: true,
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

/// Get container resource stats snapshot.
pub async fn container_stats(docker: &Docker, id: &str) -> Result<serde_json::Value> {
    use bollard::query_parameters::StatsOptions;
    use futures_util::StreamExt;

    let mut stream = docker.stats(
        id,
        Some(StatsOptions {
            stream: false,
            one_shot: true,
        }),
    );

    if let Some(Ok(stats)) = stream.next().await {
        let cpu = stats.cpu_stats.as_ref();
        let precpu = stats.precpu_stats.as_ref();

        let cpu_delta = cpu
            .and_then(|c| c.cpu_usage.as_ref())
            .and_then(|u| u.total_usage)
            .unwrap_or(0) as f64
            - precpu
                .and_then(|c| c.cpu_usage.as_ref())
                .and_then(|u| u.total_usage)
                .unwrap_or(0) as f64;

        let system_delta = cpu
            .and_then(|c| c.system_cpu_usage)
            .unwrap_or(0) as f64
            - precpu
                .and_then(|c| c.system_cpu_usage)
                .unwrap_or(0) as f64;

        let num_cpus = cpu.and_then(|c| c.online_cpus).unwrap_or(1) as f64;

        let cpu_percent = if system_delta > 0.0 {
            (cpu_delta / system_delta) * num_cpus * 100.0
        } else {
            0.0
        };

        let mem = stats.memory_stats.as_ref();
        let mem_usage = mem.and_then(|m| m.usage).unwrap_or(0);
        let mem_limit = mem.and_then(|m| m.limit).unwrap_or(1);

        Ok(serde_json::json!({
            "cpu_percent": cpu_percent,
            "memory_usage": mem_usage,
            "memory_limit": mem_limit,
            "memory_percent": (mem_usage as f64 / mem_limit as f64) * 100.0,
        }))
    } else {
        Ok(serde_json::json!({}))
    }
}
