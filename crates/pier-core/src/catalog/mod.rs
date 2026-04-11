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
    pub fields: HashMap<String, UiField>,
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
            "mysql" => 1,
            "redis" => 2,
            "mongodb" => 3,
            "mariadb" => 4,
            "clickhouse" => 5,
            "cassandra" => 6,
            "scylladb" => 7,
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
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx: usize = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

// ── Compose Builder ─────────────────────────────────────────

/// Build docker-compose.yml for a simple single-container template.
pub fn build_compose_yaml(
    item: &CatalogItem,
    service_id: &str,
    name: &str,
    env_vars: &HashMap<String, String>,
    ports: &[(String, u16, u16)], // (port_name, host_port, container_port)
    network_name: Option<&str>,
) -> String {
    let docker = match &item.docker {
        Some(d) => d,
        None => return String::new(),
    };

    let image = substitute(&docker.image, env_vars);

    let mut yaml = String::from("services:\n");
    yaml.push_str(&format!("  {}:\n", item.meta.id));
    yaml.push_str(&format!("    image: {image}\n"));
    yaml.push_str(&format!("    container_name: pier-{name}\n"));

    // Ports — bind to 127.0.0.1 (private) by default; user can toggle to 0.0.0.0 (public)
    if !ports.is_empty() {
        yaml.push_str("    ports:\n");
        for (_, host, container) in ports {
            // Default: databases private, services public (can be toggled via UI)
            let is_public = item.meta.category != "database";
            if is_public {
                yaml.push_str(&format!("      - \"{host}:{container}\"\n"));
            } else {
                yaml.push_str(&format!("      - \"127.0.0.1:{host}:{container}\"\n"));
            }
        }
    }

    // Environment — only include actual env vars (uppercase keys)
    let env_entries: Vec<_> = env_vars
        .iter()
        .filter(|(key, _)| key.chars().next().is_some_and(|c| c.is_uppercase()))
        .collect();
    if !env_entries.is_empty() {
        yaml.push_str("    environment:\n");
        for (key, val) in env_entries {
            yaml.push_str(&format!("      {key}: \"{val}\"\n"));
        }
    }

    // Volumes
    if !item.volumes.is_empty() {
        yaml.push_str("    volumes:\n");
        for (vol_name, vol) in &item.volumes {
            yaml.push_str(&format!("      - {vol_name}:{}\n", vol.mount));
        }
    }

    // Healthcheck
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

    // Service networks: always include pier-net (shared) + optional project network
    let net = network_name.unwrap_or("pier-net");
    yaml.push_str("    networks:\n");
    yaml.push_str(&format!("      - {net}\n"));
    if net != "pier-net" {
        // Also connect to pier-net so services across networks can communicate
        yaml.push_str("      - pier-net\n");
    }

    // Named volumes
    if !item.volumes.is_empty() {
        yaml.push_str("volumes:\n");
        for vol_name in item.volumes.keys() {
            yaml.push_str(&format!("  {vol_name}:\n"));
        }
    }

    // Network definitions (external — managed by Pier)
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

/// Regenerate port bindings in compose YAML based on is_public flag.
/// Replaces `127.0.0.1:port:port` ↔ `port:port` (0.0.0.0) for all port lines.
pub fn regenerate_compose_ports(yaml: &str, is_public: bool) -> String {
    yaml.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Match port binding lines like: - "127.0.0.1:10000:5432" or - "10000:5432"
            if trimmed.starts_with("- \"") && trimmed.contains(':') {
                let content = trimmed.trim_start_matches("- \"").trim_end_matches('"');
                let parts: Vec<&str> = content.split(':').collect();
                let indent = line.len() - line.trim_start().len();
                let spaces = &line[..indent];

                match parts.len() {
                    // "host_port:container_port" (currently public)
                    2 => {
                        if is_public {
                            line.to_string()
                        } else {
                            format!("{spaces}- \"127.0.0.1:{}:{}\"", parts[0], parts[1])
                        }
                    }
                    // "ip:host_port:container_port" (currently private or explicit IP)
                    3 => {
                        if is_public {
                            format!("{spaces}- \"{}:{}\"", parts[1], parts[2])
                        } else {
                            format!("{spaces}- \"127.0.0.1:{}:{}\"", parts[1], parts[2])
                        }
                    }
                    _ => line.to_string(),
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
