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

/// `(compose_service_name, public_port, container_port)` — one operator-toggled
/// public binding that needs to land on the host as `-p 0.0.0.0:public:container`.
///
/// `compose_service_name` is `Some(name)` for multi-service compose stacks (the
/// service under `services:` this binding belongs to). `None` means
/// "single-service compose / legacy port_allocation without compose_service tag";
/// the injector picks the only service in the YAML.
pub type PublicBinding = (Option<String>, u16, u16);

/// Inject `- "0.0.0.0:{public}:{container}"` port entries into an
/// externally-supplied docker-compose YAML (git-deployed services, compose
/// templates) so that the operator's "Make publicly available" toggle
/// actually publishes the port to the host.
///
/// Behavior:
/// - All existing `0.0.0.0:*:*` lines in any service's `ports:` block are
///   considered Pier-managed and removed. The provided `bindings` are then
///   re-emitted as the authoritative set, making the operation idempotent and
///   correctly handling toggle-off (empty `bindings`) for previously-published
///   ports.
/// - Other port lines (e.g. `127.0.0.1:host:container`, plain `host:container`,
///   or bindings to a non-0.0.0.0 interface) are left untouched — those belong
///   to the user or to other layers of Pier.
/// - New entries are appended to the matching service's `ports:` block,
///   preserving its existing indentation. If a target service has no `ports:`
///   block at all, one is created at the service's property indent.
///
/// The parser is line-based and intentionally tolerant: it preserves comments,
/// blank lines, and unrelated keys. It does not round-trip via `serde_yaml`
/// because that strips comments and reorders keys, which matters for
/// human-authored compose files pulled from git.
pub fn inject_public_ports_into_compose(
    yaml: &str,
    bindings: &[PublicBinding],
) -> anyhow::Result<String> {
    let mut lines: Vec<String> = yaml.lines().map(|l| l.to_string()).collect();

    // Locate top-level `services:` block.
    let services_idx = match lines
        .iter()
        .position(|l| l.trim() == "services:" && !l.starts_with(' ') && !l.starts_with('\t'))
    {
        Some(i) => i,
        None => {
            // No services: key — nothing to inject into. For empty/minimal
            // YAML we return as-is rather than fabricating a structure.
            return Ok(yaml.to_string());
        }
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
                return Some(0);
            }
            Some(indent)
        })
        .unwrap_or(0);
    if service_indent == 0 {
        return Ok(yaml.to_string());
    }

    // Collect (name, start, end) for each service block. `end` is exclusive.
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
            let name = trimmed.trim_end_matches(':').trim().to_string();
            current = Some((name, i));
        }
    }
    if let Some((n, s)) = current.take() {
        service_ranges.push((n, s, lines.len()));
    }

    if service_ranges.is_empty() {
        return Ok(yaml.to_string());
    }

    // Pre-compute, per service, which bindings apply.
    // A binding with `compose_service = Some(name)` applies to exactly that
    // service. A binding with `compose_service = None` applies to the only
    // service in the YAML (legacy single-service composes); if there are
    // multiple, we attach it to the first one to remain useful, but the API
    // layer should not produce such bindings for multi-service stacks.
    let only_one = service_ranges.len() == 1;
    let mut per_service: Vec<Vec<(u16, u16)>> = vec![Vec::new(); service_ranges.len()];
    for (svc, public, container) in bindings {
        match svc {
            Some(name) => {
                if let Some(idx) = service_ranges.iter().position(|(n, _, _)| n == name) {
                    per_service[idx].push((*public, *container));
                }
                // Silently skip bindings pointing to non-existent services —
                // the YAML may have been edited; treating it as an error would
                // wedge the toggle.
            }
            None => {
                if only_one {
                    per_service[0].push((*public, *container));
                } else if let Some(first) = per_service.first_mut() {
                    first.push((*public, *container));
                }
            }
        }
    }

    // Walk services in reverse so earlier (start, end) indices stay valid as
    // we splice lines in/out.
    for ((_, start, end), wanted) in service_ranges.iter().zip(per_service.iter()).rev() {
        let (start, end, wanted) = (*start, *end, wanted.clone());
        rewrite_service_public_ports(&mut lines, start, end, service_indent, &wanted);
    }

    let mut out = lines.join("\n");
    if yaml.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Replace all `- "0.0.0.0:*:*"` lines inside one service block with exactly
/// the entries from `wanted`. Creates a `ports:` block if the service has none
/// and `wanted` is non-empty.
fn rewrite_service_public_ports(
    lines: &mut Vec<String>,
    svc_start: usize,
    svc_end: usize,
    service_indent: usize,
    wanted: &[(u16, u16)],
) {
    // Infer the service's property indent from the first indented body line.
    let prop_indent = (svc_start + 1..svc_end)
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

    // Locate `ports:` line within this service block.
    let ports_key_idx = (svc_start + 1..svc_end).find(|&i| {
        let line = &lines[i];
        let ind = line.len() - line.trim_start().len();
        ind == prop_indent && line.trim() == "ports:"
    });

    if let Some(ports_idx) = ports_key_idx {
        // Find the range of port items (lines at `ind > prop_indent` starting
        // with `- `), starting right after the `ports:` key. We stop at the
        // first non-blank line that is not a port item.
        let mut items_end = ports_idx + 1;
        let mut item_indent: Option<usize> = None;
        while items_end < svc_end {
            let line = &lines[items_end];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                items_end += 1;
                continue;
            }
            let ind = line.len() - line.trim_start().len();
            if ind <= prop_indent {
                break;
            }
            if trimmed.starts_with("- ") || trimmed == "-" {
                if item_indent.is_none() {
                    item_indent = Some(ind);
                }
                items_end += 1;
                continue;
            }
            break;
        }
        let item_pad = item_indent.unwrap_or(prop_indent + 2);

        // Remove existing Pier-managed entries (`0.0.0.0:N:M`, with or
        // without quotes / protocol suffix). Leave everything else.
        let mut filtered: Vec<String> = Vec::new();
        for line in &lines[(ports_idx + 1)..items_end] {
            if is_public_zero_addr_port_line(line) {
                continue;
            }
            filtered.push(line.clone());
        }

        // Append the wanted entries (skipping duplicates against `filtered` —
        // shouldn't happen because we already stripped 0.0.0.0 lines, but be
        // defensive).
        let pad = " ".repeat(item_pad);
        for (public, container) in wanted {
            let entry = format!("{pad}- \"0.0.0.0:{public}:{container}\"");
            if !filtered.iter().any(|l| l.trim() == entry.trim()) {
                filtered.push(entry);
            }
        }

        // Splice: replace lines (ports_idx+1 .. items_end) with `filtered`.
        let _: Vec<String> = lines.splice(ports_idx + 1..items_end, filtered).collect();
    } else if !wanted.is_empty() {
        // No ports: block — synthesize one. Insert it just before any trailing
        // blank lines at the end of the service block.
        let mut insert_at = svc_end;
        while insert_at > svc_start + 1 && lines[insert_at - 1].trim().is_empty() {
            insert_at -= 1;
        }
        let key_pad = " ".repeat(prop_indent);
        let item_pad = " ".repeat(prop_indent + 2);
        let mut new_lines = vec![format!("{key_pad}ports:")];
        for (public, container) in wanted {
            new_lines.push(format!("{item_pad}- \"0.0.0.0:{public}:{container}\""));
        }
        let _: Vec<String> = lines.splice(insert_at..insert_at, new_lines).collect();
    }
}

/// True if the line is a `ports:` list entry of the form
/// `- "0.0.0.0:N:M"` / `- 0.0.0.0:N:M` (optionally with a `/tcp`/`/udp`
/// suffix and any surrounding whitespace).
fn is_public_zero_addr_port_line(line: &str) -> bool {
    let trimmed = line.trim();
    let rest = match trimmed.strip_prefix("- ") {
        Some(r) => r,
        None => return false,
    };
    let cleaned = rest.trim().trim_matches('"').trim_matches('\'');
    // Strip optional /tcp or /udp suffix for the address check.
    let addr_part = cleaned.split('/').next().unwrap_or(cleaned);
    if !addr_part.starts_with("0.0.0.0:") {
        return false;
    }
    // Must be `0.0.0.0:N:M` with N and M numeric. Anything else (e.g. an IPv6
    // wildcard, env var like `0.0.0.0:${PUB}:1883`) is left alone — the user
    // wrote it deliberately.
    let parts: Vec<&str> = addr_part.split(':').collect();
    if parts.len() != 3 {
        return false;
    }
    parts[1].parse::<u16>().is_ok() && parts[2].parse::<u16>().is_ok()
}

#[cfg(test)]
mod inject_tests {
    use super::*;

    #[test]
    fn adds_binding_to_single_service_no_ports() {
        let yaml = "services:\n  api:\n    image: foo\n    restart: unless-stopped\n";
        let out = inject_public_ports_into_compose(yaml, &[(None, 1883, 1883)]).expect("inject");
        assert!(
            out.contains("ports:"),
            "ports block should be created: {out}"
        );
        assert!(
            out.contains("- \"0.0.0.0:1883:1883\""),
            "public binding missing: {out}"
        );
    }

    #[test]
    fn appends_to_existing_ports_block() {
        let yaml = "services:\n  api:\n    image: foo\n    ports:\n      - \"127.0.0.1:8080:8080\"\n    restart: unless-stopped\n";
        let out = inject_public_ports_into_compose(yaml, &[(None, 4471, 4471)]).expect("inject");
        assert!(out.contains("- \"127.0.0.1:8080:8080\""));
        assert!(out.contains("- \"0.0.0.0:4471:4471\""));
    }

    #[test]
    fn idempotent_on_repeated_run() {
        let yaml =
            "services:\n  api:\n    image: foo\n    ports:\n      - \"127.0.0.1:8080:8080\"\n";
        let once = inject_public_ports_into_compose(yaml, &[(None, 1883, 1883)]).expect("once");
        let twice = inject_public_ports_into_compose(&once, &[(None, 1883, 1883)]).expect("twice");
        assert_eq!(once, twice, "idempotent: {once}\n---\n{twice}");
    }

    #[test]
    fn removes_orphaned_public_binding_when_disabled() {
        let yaml = "services:\n  api:\n    image: foo\n    ports:\n      - \"127.0.0.1:8080:8080\"\n      - \"0.0.0.0:1883:1883\"\n";
        let out = inject_public_ports_into_compose(yaml, &[]).expect("inject");
        assert!(out.contains("- \"127.0.0.1:8080:8080\""));
        assert!(!out.contains("0.0.0.0:1883:1883"), "orphan kept: {out}");
    }

    #[test]
    fn multi_service_routes_by_name() {
        let yaml = concat!(
            "services:\n",
            "  api:\n",
            "    image: foo\n",
            "    ports:\n",
            "      - \"127.0.0.1:8080:8080\"\n",
            "  worker:\n",
            "    image: bar\n",
            "    ports:\n",
            "      - \"127.0.0.1:9000:9000\"\n",
        );
        let out = inject_public_ports_into_compose(yaml, &[(Some("worker".into()), 1883, 1883)])
            .expect("inject");
        // The worker block gets the public binding; the api block does not.
        let api_idx = out.find("api:").expect("api block");
        let worker_idx = out.find("worker:").expect("worker block");
        let api_block = &out[api_idx..worker_idx];
        let worker_block = &out[worker_idx..];
        assert!(
            !api_block.contains("0.0.0.0:1883:1883"),
            "leaked into api: {api_block}"
        );
        assert!(
            worker_block.contains("- \"0.0.0.0:1883:1883\""),
            "worker missing binding: {worker_block}"
        );
    }

    #[test]
    fn preserves_user_zero_addr_with_env_var() {
        // A user-authored `0.0.0.0:${PORT}:8080` is NOT recognized as
        // Pier-managed (non-numeric) and stays in place.
        let yaml =
            "services:\n  api:\n    image: foo\n    ports:\n      - \"0.0.0.0:${PORT}:8080\"\n";
        let out = inject_public_ports_into_compose(yaml, &[]).expect("inject");
        assert!(out.contains("${PORT}"), "user binding stripped: {out}");
    }

    #[test]
    fn replaces_old_public_with_new_public() {
        let yaml = "services:\n  api:\n    image: foo\n    ports:\n      - \"0.0.0.0:1883:1883\"\n";
        let out = inject_public_ports_into_compose(yaml, &[(None, 1888, 1883)]).expect("inject");
        assert!(!out.contains("0.0.0.0:1883:1883"), "old kept: {out}");
        assert!(out.contains("- \"0.0.0.0:1888:1883\""));
    }

    #[test]
    fn no_services_key_returns_unchanged() {
        let yaml = "version: '3'\n# nothing here\n";
        let out = inject_public_ports_into_compose(yaml, &[(None, 1883, 1883)]).expect("inject");
        assert_eq!(out, yaml);
    }
}
