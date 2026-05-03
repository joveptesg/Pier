use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::AppResult;
use crate::state::SharedState;

/// GET /api/v1/canvas — all data needed for canvas architect view.
pub async fn get_canvas(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Collect all DB data, then drop lock for async Docker calls
    let (resources, servers, networks, positions) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        // Resources with network and server info
        let mut stmt = db.prepare(
            "SELECT s.id, s.name, s.status, s.catalog_id, s.category, s.port, s.image,
                    s.network_id, n.name, s.server_id, s.project_id, s.env_json, p.name, s.container_id
             FROM services s
             LEFT JOIN networks n ON s.network_id = n.id
             LEFT JOIN projects p ON s.project_id = p.id
             ORDER BY s.name",
        )?;
        let resources: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "status": row.get::<_, String>(2)?,
                    "catalog_id": row.get::<_, Option<String>>(3)?,
                    "category": row.get::<_, Option<String>>(4)?,
                    "port": row.get::<_, Option<i64>>(5)?,
                    "image": row.get::<_, Option<String>>(6)?,
                    "network_id": row.get::<_, Option<String>>(7)?,
                    "network_name": row.get::<_, Option<String>>(8)?,
                    "server_id": row.get::<_, Option<String>>(9)?,
                    "project_id": row.get::<_, Option<String>>(10)?,
                    "env_json": row.get::<_, Option<String>>(11)?,
                    "project_name": row.get::<_, Option<String>>(12)?,
                    "container_id": row.get::<_, Option<String>>(13)?,
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Servers
        let mut stmt = db.prepare(
            "SELECT id, name, host, status, is_local, cpu_count, memory_total, docker_version, country, city, country_code
             FROM servers ORDER BY is_local DESC, name",
        )?;
        let servers: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "host": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "is_local": row.get::<_, i64>(4)? != 0,
                    "cpu_count": row.get::<_, Option<i64>>(5)?,
                    "memory_total": row.get::<_, Option<i64>>(6)?,
                    "docker_version": row.get::<_, Option<String>>(7)?,
                    "country": row.get::<_, Option<String>>(8)?,
                    "city": row.get::<_, Option<String>>(9)?,
                    "country_code": row.get::<_, Option<String>>(10)?,
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Networks
        let mut stmt =
            db.prepare("SELECT id, name, is_default FROM networks ORDER BY is_default DESC, name")?;
        let networks: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "is_default": row.get::<_, i64>(2)? != 0,
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Canvas positions
        let mut stmt = db.prepare("SELECT service_id, x, y FROM canvas_positions")?;
        let positions: std::collections::HashMap<String, serde_json::Value> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    serde_json::json!({
                        "x": row.get::<_, f64>(1)?,
                        "y": row.get::<_, f64>(2)?,
                    }),
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        (resources, servers, networks, positions)
    }; // DB lock dropped here

    // SEC-002: detect dependencies server-side via Docker inspect (don't expose env vars to frontend)
    //
    // Detection rules:
    //  - For each running container collect its env vars and DNS-reachable hostnames
    //    (container name, configured hostname, network aliases, plus Pier slug fallbacks).
    //  - An edge `source → target` is created only if a value of some env var on the source
    //    references a target hostname in URL/host:port context (see
    //    `value_references_host_in_url`). Plain substring matches like `DB_NAME=evroplast`
    //    do NOT create an edge — they used to produce false dep arrows.
    //  - The edge label is derived from the *target* service (image / catalog_id / category),
    //    not from the env var key on the source.
    let mut dep_edges: Vec<serde_json::Value> = Vec::new();

    struct Runtime {
        env_list: Vec<String>,
        hosts: Vec<String>,
    }
    struct Meta {
        image: Option<String>,
        catalog_id: Option<String>,
        category: Option<String>,
    }

    let meta: std::collections::HashMap<String, Meta> = resources
        .iter()
        .filter_map(|r| {
            let id = r.get("id")?.as_str()?.to_string();
            Some((
                id,
                Meta {
                    image: r.get("image").and_then(|v| v.as_str()).map(String::from),
                    catalog_id: r
                        .get("catalog_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    category: r.get("category").and_then(|v| v.as_str()).map(String::from),
                },
            ))
        })
        .collect();

    // Inspect each container once: collect env vars and host aliases.
    let mut runtime: std::collections::HashMap<String, Runtime> = std::collections::HashMap::new();
    for r in &resources {
        let source_id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let cn = r.get("container_id").and_then(|v| v.as_str()).unwrap_or("");
        let slug = r
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase()
            .replace(' ', "-");
        if source_id.is_empty() || cn.is_empty() {
            continue;
        }

        let info = match state.docker.inspect_container(cn, None).await {
            Ok(i) => i,
            Err(_) => continue,
        };
        let env_list = info
            .config
            .as_ref()
            .and_then(|c| c.env.clone())
            .unwrap_or_default();

        let mut hosts: Vec<String> = Vec::new();
        if let Some(name) = info.name.as_ref() {
            let n = name.trim_start_matches('/').to_lowercase();
            if !n.is_empty() {
                hosts.push(n);
            }
        }
        if let Some(cfg) = info.config.as_ref() {
            if let Some(h) = cfg.hostname.as_ref() {
                if !h.is_empty() {
                    hosts.push(h.to_lowercase());
                }
            }
        }
        if let Some(ns) = info.network_settings.as_ref() {
            if let Some(nets) = ns.networks.as_ref() {
                for net in nets.values() {
                    if let Some(aliases) = net.aliases.as_ref() {
                        for a in aliases {
                            if !a.is_empty() {
                                hosts.push(a.to_lowercase());
                            }
                        }
                    }
                }
            }
        }
        if !slug.is_empty() {
            hosts.push(slug.clone());
            hosts.push(format!("pier-{slug}"));
        }
        hosts.sort();
        hosts.dedup();

        runtime.insert(source_id.to_string(), Runtime { env_list, hosts });
    }

    // Build edges: source mentions target hostname either in URL/host:port context,
    // or as a bare hostname value in a host-typed env var (POSTGRES_HOST, REDIS_URL, ...).
    for (source_id, src) in &runtime {
        let mut found: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in &src.env_list {
            let (key, val) = match entry.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            let val_lower = val.to_lowercase();
            let key_lower = key.to_lowercase();
            let host_typed_key = is_host_like_key(&key_lower);
            for (tid, trt) in &runtime {
                if tid == source_id || found.contains(tid) {
                    continue;
                }
                let hit = trt.hosts.iter().any(|h| {
                    value_references_host_in_url(&val_lower, h)
                        || (host_typed_key && value_starts_with_host(&val_lower, h))
                });
                if hit {
                    found.insert(tid.clone());
                }
            }
        }
        for tid in found {
            let label = match meta.get(&tid) {
                Some(m) => infer_label_from_target(
                    m.image.as_deref(),
                    m.catalog_id.as_deref(),
                    m.category.as_deref(),
                ),
                None => "HTTP",
            };
            dep_edges.push(serde_json::json!({
                "from": source_id,
                "to": tid,
                "label": label,
            }));
        }
    }

    // Remove env_json from resources before sending to frontend
    let resources: Vec<serde_json::Value> = resources
        .into_iter()
        .map(|mut r| {
            r.as_object_mut().map(|o| o.remove("env_json"));
            r
        })
        .collect();

    // System metrics
    let sys = sysinfo::System::new_all();
    let cpu_percent = sys.global_cpu_usage();
    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let mem_percent = if mem_total > 0 {
        (mem_used as f64 / mem_total as f64 * 100.0) as f32
    } else {
        0.0
    };

    Ok(Json(serde_json::json!({
        "resources": resources,
        "dep_edges": dep_edges,
        "servers": servers,
        "networks": networks,
        "positions": positions,
        "system": {
            "cpu_percent": cpu_percent,
            "memory_percent": mem_percent,
            "memory_used": mem_used,
            "memory_total": mem_total,
        }
    })))
}

/// PUT /api/v1/canvas/positions — save card positions after drag.
pub async fn save_positions(
    State(state): State<SharedState>,
    Json(body): Json<Vec<PositionUpdate>>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    for pos in &body {
        db.execute(
            "INSERT INTO canvas_positions (service_id, x, y, updated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(service_id) DO UPDATE SET x = ?2, y = ?3, updated_at = datetime('now')",
            rusqlite::params![pos.service_id, pos.x, pos.y],
        )?;
    }

    Ok(Json(serde_json::json!({"ok": true, "saved": body.len()})))
}

#[derive(Deserialize)]
pub struct PositionUpdate {
    pub service_id: String,
    pub x: f64,
    pub y: f64,
}

/// Pick a friendly dep-arrow label based on the *target* service.
/// Order: catalog_id (exact) → image (substring) → category → "HTTP".
fn infer_label_from_target(
    image: Option<&str>,
    catalog_id: Option<&str>,
    category: Option<&str>,
) -> &'static str {
    let cat_id = catalog_id.unwrap_or("").to_lowercase();
    match cat_id.as_str() {
        "postgresql" | "postgres" => return "PostgreSQL",
        "redis" => return "Redis",
        "mongodb" | "mongo" => return "MongoDB",
        "rabbitmq" => return "RabbitMQ",
        "mysql" => return "MySQL",
        "mariadb" => return "MariaDB",
        "clickhouse" => return "ClickHouse",
        "elasticsearch" => return "Elasticsearch",
        _ => {}
    }
    let img = image.unwrap_or("").to_lowercase();
    if img.contains("postgres") {
        return "PostgreSQL";
    }
    if img.contains("redis") {
        return "Redis";
    }
    if img.contains("mongo") {
        return "MongoDB";
    }
    if img.contains("rabbitmq") || img.contains("amqp") {
        return "RabbitMQ";
    }
    if img.contains("mariadb") {
        return "MariaDB";
    }
    if img.contains("mysql") {
        return "MySQL";
    }
    if img.contains("clickhouse") {
        return "ClickHouse";
    }
    if img.contains("elastic") {
        return "Elasticsearch";
    }
    match category.unwrap_or("").to_lowercase().as_str() {
        "database" => "Database",
        "cache" => "Cache",
        "queue" | "broker" => "Queue",
        _ => "HTTP",
    }
}

/// Returns true iff `host` appears in `val_lower` as the host part of a URL or
/// in host:port form. Plain bare-string equality (e.g. `DB_NAME=evroplast`) does
/// NOT count — that used to produce false dep arrows.
///
/// Match requires:
/// - hostname-style word boundaries on both sides (non-alnum/`-`/`_`/`.`),
/// - AND one of these adjacency contexts:
///   - preceded by `@`            (URL with userinfo)
///   - preceded by `://`          (URL without userinfo)
///   - followed by `:` then digit (host:port form like `redis:6379`)
fn value_references_host_in_url(val_lower: &str, host: &str) -> bool {
    if host.is_empty() || val_lower.len() < host.len() {
        return false;
    }
    let bytes = val_lower.as_bytes();
    let h = host.as_bytes();
    let mut i = 0;
    while i + h.len() <= bytes.len() {
        if &bytes[i..i + h.len()] == h {
            let left_ok = i == 0 || !is_hostchar(bytes[i - 1]);
            let after = i + h.len();
            let right_ok = after == bytes.len() || !is_hostchar(bytes[after]);
            if left_ok && right_ok {
                let after_at = i > 0 && bytes[i - 1] == b'@';
                let after_scheme = i >= 3 && &bytes[i - 3..i] == b"://";
                let before_port = after + 1 < bytes.len()
                    && bytes[after] == b':'
                    && bytes[after + 1].is_ascii_digit();
                if after_at || after_scheme || before_port {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn is_hostchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// True if the env var key signals "this value is a host/URL/connection target",
/// e.g. `POSTGRES_HOST`, `REDIS_URL`, `RABBITMQ_URI`, `DATABASE_DSN`. Used as a
/// gate for accepting bare hostname values (which would otherwise create false
/// edges from values like `DB_NAME=evroplast`).
fn is_host_like_key(key_lower: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "_host",
        "_hostname",
        "_server",
        "_addr",
        "_address",
        "_endpoint",
        "_url",
        "_uri",
        "_dsn",
        "_link",
        "_connection",
    ];
    if SUFFIXES.iter().any(|s| key_lower.ends_with(s)) {
        return true;
    }
    matches!(
        key_lower,
        "host" | "hostname" | "server" | "url" | "uri" | "dsn" | "endpoint"
    )
}

/// True if `val_lower` begins with `host` followed by a hostname boundary
/// (end-of-string or non-alnum/`-`/`_`/`.`). Catches `postgresql`, `postgresql:5432`,
/// `pier-redis/0`, but not `postgresqltest` or `postgresql-replica`.
fn value_starts_with_host(val_lower: &str, host: &str) -> bool {
    if host.is_empty() || !val_lower.starts_with(host) {
        return false;
    }
    let after = host.len();
    after == val_lower.len() || !is_hostchar(val_lower.as_bytes()[after])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_from_postgres_image() {
        assert_eq!(
            infer_label_from_target(Some("postgres:latest"), None, None),
            "PostgreSQL"
        );
    }

    #[test]
    fn label_from_redis_catalog_id() {
        assert_eq!(infer_label_from_target(None, Some("redis"), None), "Redis");
    }

    #[test]
    fn label_unknown_app_falls_back_to_http() {
        assert_eq!(
            infer_label_from_target(Some("nginx:1.27"), None, None),
            "HTTP"
        );
        assert_eq!(infer_label_from_target(None, None, None), "HTTP");
    }

    #[test]
    fn label_from_database_category() {
        assert_eq!(
            infer_label_from_target(None, None, Some("database")),
            "Database"
        );
    }

    #[test]
    fn label_catalog_wins_over_image() {
        assert_eq!(
            infer_label_from_target(Some("nginx:1.27"), Some("postgresql"), None),
            "PostgreSQL"
        );
    }

    #[test]
    fn host_match_after_at_in_url() {
        assert!(value_references_host_in_url(
            "postgres://u:p@pier-postgresql:5432/db",
            "pier-postgresql"
        ));
    }

    #[test]
    fn host_match_after_scheme() {
        assert!(value_references_host_in_url(
            "redis://pier-redis:6379",
            "pier-redis"
        ));
    }

    #[test]
    fn host_match_host_port_form() {
        assert!(value_references_host_in_url(
            "amqp://rabbit:5672/",
            "rabbit"
        ));
    }

    #[test]
    fn host_no_match_path_segment() {
        // db name "evroplast" sits in the URL path — must NOT match.
        assert!(!value_references_host_in_url(
            "postgres://u:p@pier-postgresql:5432/evroplast",
            "evroplast"
        ));
    }

    #[test]
    fn host_no_match_bare_value() {
        // DB_NAME=evroplast — value equals service name but no URL context.
        assert!(!value_references_host_in_url("evroplast", "evroplast"));
    }

    #[test]
    fn host_no_match_inside_longer_token() {
        assert!(!value_references_host_in_url(
            "http://evroplast-web:80",
            "evroplast"
        ));
    }

    #[test]
    fn host_like_keys() {
        assert!(is_host_like_key("postgres_host"));
        assert!(is_host_like_key("redis_url"));
        assert!(is_host_like_key("rabbitmq_uri"));
        assert!(is_host_like_key("database_dsn"));
        assert!(is_host_like_key("api_endpoint"));
        assert!(is_host_like_key("host"));
        assert!(is_host_like_key("url"));
        assert!(!is_host_like_key("db_name"));
        assert!(!is_host_like_key("postgres_password"));
        assert!(!is_host_like_key("postgres_user"));
    }

    #[test]
    fn value_starts_with_host_bare() {
        assert!(value_starts_with_host("postgresql", "postgresql"));
        assert!(value_starts_with_host("postgresql:5432", "postgresql"));
        assert!(value_starts_with_host("pier-redis/0", "pier-redis"));
        assert!(!value_starts_with_host("postgresqltest", "postgresql"));
        assert!(!value_starts_with_host("postgresql-replica", "postgresql"));
        assert!(!value_starts_with_host("evroplast", "postgresql"));
    }
}
