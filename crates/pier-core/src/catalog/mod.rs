pub mod cluster;

use std::collections::HashMap;

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

#[derive(RustEmbed)]
#[folder = "../../templates/"]
struct CatalogAssets;

// ── TOML Template Structures ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogItem {
    pub meta: CatalogMeta,
    pub docker: Option<DockerConfig>,
    pub compose: Option<ComposeConfig>,
    #[serde(default)]
    pub ports: HashMap<String, PortConfig>,
    #[serde(default)]
    pub volumes: HashMap<String, VolumeConfig>,
    #[serde(default)]
    pub env: HashMap<String, EnvVar>,
    pub healthcheck: Option<HealthcheckConfig>,
    pub versions: Option<VersionsConfig>,
    pub ui: Option<UiConfig>,
    pub cluster: Option<ClusterConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogMeta {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub icon: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    pub image: String,
    #[serde(default)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeConfig {
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortConfig {
    pub internal: u16,
    #[serde(default = "default_tcp")]
    pub protocol: String,
    pub description: Option<String>,
}

fn default_tcp() -> String {
    "tcp".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeConfig {
    pub mount: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVar {
    pub default: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthcheckConfig {
    pub test: Vec<String>,
    #[serde(default = "default_interval")]
    pub interval: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default = "default_retries")]
    pub retries: u32,
    pub start_period: Option<String>,
}

fn default_interval() -> String {
    "10s".to_string()
}
fn default_timeout() -> String {
    "5s".to_string()
}
fn default_retries() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionsConfig {
    pub available: Vec<String>,
    pub default: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub fields: indexmap::IndexMap<String, UiField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiField {
    #[serde(rename = "type")]
    pub field_type: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
    pub placeholder: Option<String>,
    pub options_from: Option<String>,
    pub maps_to: Option<String>,
    #[serde(default)]
    pub auto_generate: bool,
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    #[serde(default)]
    pub supported: bool,
    #[serde(default = "default_min_nodes")]
    pub min_nodes: usize,
    #[serde(default = "default_max_nodes")]
    pub max_nodes: usize,
    #[serde(default = "default_nodes")]
    pub default_nodes: usize,
    #[serde(default)]
    pub description: String,
}

fn default_min_nodes() -> usize {
    2
}
fn default_max_nodes() -> usize {
    5
}
fn default_nodes() -> usize {
    3
}

// ── Loading ─────────────────────────────────────────────────

/// Load all catalog templates from embedded TOML files.
pub fn load_catalog() -> Vec<CatalogItem> {
    let mut items = Vec::new();

    for path in CatalogAssets::iter() {
        if !path.ends_with(".toml") {
            continue;
        }
        if let Some(file) = CatalogAssets::get(&path) {
            match std::str::from_utf8(file.data.as_ref()) {
                Ok(content) => match toml::from_str::<CatalogItem>(content) {
                    Ok(item) => {
                        tracing::info!("Loaded catalog template: {} ({})", item.meta.id, path);
                        items.push(item);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse catalog template {path}: {e}");
                    }
                },
                Err(e) => {
                    tracing::warn!("Invalid UTF-8 in catalog template {path}: {e}");
                }
            }
        }
    }

    // Sort: git first, then applications, then databases (by popularity), then services
    items.sort_by(|a, b| {
        let cat_order = |cat: &str| match cat {
            "git" => 0,
            "application" => 1,
            "database" => 2,
            "service" => 3,
            _ => 4,
        };
        let db_popularity = |id: &str| match id {
            "postgresql" => 0,
            "postgis" => 1,
            "mysql" => 2,
            "redis" => 3,
            "mongodb" => 4,
            "mariadb" => 5,
            "clickhouse" => 6,
            "cassandra" => 7,
            "scylladb" => 8,
            _ => 99,
        };
        cat_order(&a.meta.category)
            .cmp(&cat_order(&b.meta.category))
            .then_with(|| {
                if a.meta.category == "database" && b.meta.category == "database" {
                    db_popularity(&a.meta.id).cmp(&db_popularity(&b.meta.id))
                } else if a.meta.category == "service" && b.meta.category == "service" {
                    let svc = |id: &str| match id {
                        "supabase" => 0,
                        "grafana" => 1,
                        "grafana-postgresql" => 2,
                        "elasticsearch" => 3,
                        "elasticsearch-kibana" => 4,
                        "rabbitmq" => 5,
                        "qdrant" => 6,
                        "portainer" => 7,
                        "gitea" => 8,
                        "gitea-postgresql" => 9,
                        "directus" => 10,
                        "directus-postgresql" => 11,
                        "nocobase" => 12,
                        "matrix-synapse-postgresql" => 13,
                        "matrix-synapse-sqlite" => 14,
                        "beszel" => 15,
                        "gotify" => 16,
                        "audiobookshelf" => 17,
                        "amneziawg" => 18,
                        "minecraft" => 19,
                        "terraria" => 20,
                        _ => 99,
                    };
                    svc(&a.meta.id).cmp(&svc(&b.meta.id))
                } else {
                    a.meta.name.cmp(&b.meta.name)
                }
            })
    });

    items
}

// ── Substitution ────────────────────────────────────────────

/// Replace `{{key}}` placeholders in a template string.
pub fn substitute(template: &str, vars: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        let pattern = format!("{{{{{key}}}}}");
        result = result.replace(&pattern, value);
    }
    result
}

/// Generate a random password of the given length.
pub fn generate_password(len: usize) -> String {
    use rand::RngExt;
    // URL-safe charset: upper + lower + digits + safe chars only (no special chars that break URLs/SQL/shell)
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len.max(24))
        .map(|_| {
            let idx: usize = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

// ── Compose Builder ─────────────────────────────────────────

/// Build docker-compose.yml for a simple single-container template.
///
/// Thin wrapper over `build_compose_yaml_scaled` for backward compatibility.
/// Generates a single replica bound to `127.0.0.1` (main-server default).
pub fn build_compose_yaml(
    item: &CatalogItem,
    service_id: &str,
    name: &str,
    env_vars: &HashMap<String, String>,
    ports: &[ReplicaPortMapping],
    network_name: Option<&str>,
) -> String {
    build_compose_yaml_scaled(
        item,
        service_id,
        name,
        env_vars,
        &[(1, ports.to_vec())],
        network_name,
        false,
    )
}

/// `(port_name, host_port, container_port, public_port)` — one published port of a replica.
///
/// `public_port` is `Some(p)` when the operator toggled "Make publicly available"
/// for this port; emitted as an extra `0.0.0.0:{p}:{container_port}` Docker port
/// binding alongside the always-on `127.0.0.1:{host_port}:{container_port}` mapping.
/// `None` means no public exposure (internal Docker network access only).
pub type ReplicaPortMapping = (String, u16, u16, Option<u16>);

/// `(replica_index, port_mappings)` — one scaled replica and its ports.
pub type ReplicaSlot = (i64, Vec<ReplicaPortMapping>);

/// Build docker-compose.yml with N replicas.
///
/// - `replicas`: `[(replica_idx, ports_for_this_replica)]`. Each entry produces
///   one compose service. When `replicas.len() == 1`, the service key and
///   container name keep the legacy (no-suffix) form to preserve compatibility
///   with existing deployments.
/// - `bind_public`: when `true`, the **base** `host_port:container_port` mapping
///   binds to `0.0.0.0` (used on remote servers reached from the main Traefik);
///   when `false`, `127.0.0.1`. Independent of the per-port `public_port` flag —
///   `public_port` always adds a separate `0.0.0.0:{public}:{container}` line
///   so the operator-chosen public port is exposed even on local main-server.
pub fn build_compose_yaml_scaled(
    item: &CatalogItem,
    service_id: &str,
    name: &str,
    env_vars: &HashMap<String, String>,
    replicas: &[ReplicaSlot],
    network_name: Option<&str>,
    bind_public: bool,
) -> String {
    let docker = match &item.docker {
        Some(d) => d,
        None => return String::new(),
    };

    let image = substitute(&docker.image, env_vars);
    let net = network_name.unwrap_or("pier-net");
    let bind_addr = if bind_public { "0.0.0.0" } else { "127.0.0.1" };
    let single_replica = replicas.len() == 1;

    let cmd_entries: Vec<String> = docker
        .command
        .iter()
        .map(|c| format!("\"{}\"", substitute(c, env_vars)))
        .collect();

    let env_entries: Vec<_> = env_vars
        .iter()
        .filter(|(key, _)| key.chars().next().is_some_and(|c| c.is_uppercase()))
        .collect();

    let mut yaml = String::from("services:\n");

    for (idx, ports) in replicas {
        let (svc_key, container_name) = if single_replica {
            (item.meta.id.clone(), format!("pier-{name}"))
        } else {
            (
                format!("{}_{}", item.meta.id, idx),
                format!("pier-{name}-{idx}"),
            )
        };

        yaml.push_str(&format!("  {svc_key}:\n"));
        yaml.push_str(&format!("    image: {image}\n"));
        yaml.push_str(&format!("    container_name: {container_name}\n"));

        if !cmd_entries.is_empty() {
            yaml.push_str(&format!("    command: [{}]\n", cmd_entries.join(", ")));
        }

        if !ports.is_empty() {
            yaml.push_str("    ports:\n");
            for (_, host, container, public) in ports {
                yaml.push_str(&format!("      - \"{bind_addr}:{host}:{container}\"\n"));
                // When the operator marked this port public, expose it on the
                // requested public port on 0.0.0.0 directly via Docker — no
                // Traefik TCP routing needed. Skip if it would duplicate the
                // base binding (remote-server `0.0.0.0:host:container` with the
                // same `public == host`).
                if let Some(p) = public {
                    if !(bind_public && *p == *host) {
                        yaml.push_str(&format!("      - \"0.0.0.0:{p}:{container}\"\n"));
                    }
                }
            }
        }

        if !env_entries.is_empty() {
            yaml.push_str("    environment:\n");
            for (key, val) in &env_entries {
                yaml.push_str(&format!("      {key}: \"{val}\"\n"));
            }
        }

        if !item.volumes.is_empty() {
            yaml.push_str("    volumes:\n");
            for (vol_name, vol) in &item.volumes {
                yaml.push_str(&format!("      - {vol_name}:{}\n", vol.mount));
            }
        }

        if let Some(hc) = &item.healthcheck {
            yaml.push_str("    healthcheck:\n");
            let test_str = serde_json::to_string(&hc.test).unwrap_or_default();
            yaml.push_str(&format!("      test: {test_str}\n"));
            yaml.push_str(&format!("      interval: {}\n", hc.interval));
            yaml.push_str(&format!("      timeout: {}\n", hc.timeout));
            yaml.push_str(&format!("      retries: {}\n", hc.retries));
            if let Some(sp) = &hc.start_period {
                yaml.push_str(&format!("      start_period: {sp}\n"));
            }
        }

        yaml.push_str("    restart: unless-stopped\n");
        yaml.push_str("    labels:\n");
        yaml.push_str(&format!("      pier.service.id: \"{service_id}\"\n"));
        yaml.push_str(&format!("      pier.catalog.id: \"{}\"\n", item.meta.id));
        yaml.push_str(&format!("      pier.replica.idx: \"{idx}\"\n"));

        yaml.push_str("    networks:\n");
        yaml.push_str(&format!("      - {net}\n"));
        if net != "pier-net" {
            yaml.push_str("      - pier-net\n");
        }
    }

    if !item.volumes.is_empty() {
        yaml.push_str("volumes:\n");
        for vol_name in item.volumes.keys() {
            yaml.push_str(&format!("  {vol_name}:\n"));
        }
    }

    yaml.push_str("networks:\n");
    yaml.push_str(&format!("  {net}:\n"));
    yaml.push_str("    external: true\n");
    if net != "pier-net" {
        yaml.push_str("  pier-net:\n");
        yaml.push_str("    external: true\n");
    }

    yaml
}

/// Build docker-compose.yml from a compose template (multi-container).
pub fn build_from_template(template: &str, vars: &HashMap<String, String>) -> String {
    substitute(template, vars)
}
