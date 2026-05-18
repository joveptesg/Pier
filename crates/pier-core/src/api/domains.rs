use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, GlobalRole, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::proxy::config::{self, DomainTarget};
use crate::state::{AppState, SharedState};

#[derive(Deserialize)]
pub struct CreateDomainRequest {
    pub domain: String,
    pub service_id: String,
    /// When set, the domain routes to this specific compose-service inside
    /// a multi-service docker-compose deployment (e.g. one of N containers
    /// in the stack). `None` keeps the legacy single-target behavior.
    #[serde(default)]
    pub compose_service: Option<String>,
    /// Forward the path prefix to the upstream when `false`. Default `true`
    /// matches historical behavior: Pier emits a Traefik `stripPrefix`
    /// middleware so e.g. `example.com/api/x` becomes `/x` at the backend.
    /// Set `false` for backends whose own router expects the same prefix
    /// (Telegram-style webhooks, sub-mounted APIs).
    #[serde(default = "default_strip_prefix")]
    pub strip_prefix: bool,
}

fn default_strip_prefix() -> bool {
    true
}

/// GET /api/v1/domains
///
/// Global Admin+ and peers see every domain. Plain Users see only domains
/// whose service belongs to a project they're a member of.
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let see_all = user.is_peer || user.global_role.at_least(GlobalRole::Admin);
    let row_to_json = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "id": row.get::<_, String>(0)?,
            "domain": row.get::<_, String>(1)?,
            "service_id": row.get::<_, String>(2)?,
            "ssl_status": row.get::<_, String>(3)?,
            "ssl_expires_at": row.get::<_, Option<String>>(4)?,
            "ssl_provider": row.get::<_, String>(5)?,
            "is_generated": row.get::<_, i32>(6)? != 0,
            "created_at": row.get::<_, String>(7)?,
            "compose_service": row.get::<_, Option<String>>(8)?,
            "strip_prefix": row.get::<_, i32>(9)? != 0,
            "service_name": row.get::<_, Option<String>>(10)?,
        }))
    };
    let items: Vec<serde_json::Value> = if see_all {
        let mut stmt = db.prepare(
            "SELECT d.id, d.domain, d.service_id, d.ssl_status, d.ssl_expires_at,
                    d.ssl_provider, d.is_generated, d.created_at, d.compose_service,
                    d.strip_prefix, s.name as service_name
             FROM domains d
             LEFT JOIN services s ON d.service_id = s.id
             ORDER BY d.created_at DESC",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([], row_to_json)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        let mut stmt = db.prepare(
            "SELECT d.id, d.domain, d.service_id, d.ssl_status, d.ssl_expires_at,
                    d.ssl_provider, d.is_generated, d.created_at, d.compose_service,
                    d.strip_prefix, s.name as service_name
             FROM domains d
             JOIN services s ON d.service_id = s.id
             JOIN project_members pm ON pm.project_id = s.project_id
             WHERE pm.user_id = ?1
             ORDER BY d.created_at DESC",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([&user.id], row_to_json)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    Ok(Json(items))
}

/// POST /api/v1/domains
pub async fn create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<CreateDomainRequest>,
) -> AppResult<impl IntoResponse> {
    // Need Editor on the service's project before we even touch DB.
    enforce_resource_role(&state, &user, &body.service_id, ProjectRole::Editor)?;
    // Parse full URL: extract hostname and optional path prefix
    // Input examples: "https://api.example.com/v1", "api.example.com", "http://example.com/api/v2"
    let mut raw = body.domain.trim().to_lowercase();
    raw = raw.strip_prefix("https://").unwrap_or(&raw).to_string();
    raw = raw.strip_prefix("http://").unwrap_or(&raw).to_string();
    raw = raw.trim_end_matches('/').to_string();

    let (domain, path_prefix) = if let Some(slash_pos) = raw.find('/') {
        (raw[..slash_pos].to_string(), raw[slash_pos..].to_string())
    } else {
        (raw.clone(), String::new())
    };
    let domain = domain.trim_end_matches('.').to_string();

    if domain.is_empty() {
        return Err(AppError::BadRequest("Domain is required".into()));
    }

    // Validate the service exists.
    let service_name: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1",
            [&body.service_id],
            |row| row.get(0),
        )
        .map_err(|_| AppError::NotFound(format!("Service {} not found", body.service_id)))?
    };

    // Build the upstream URL for THIS domain so we can fail fast if the
    // requested compose_service is unknown / has no port.
    let target_url = build_target_url(&state, &body.service_id, body.compose_service.as_deref())?;

    let id = uuid::Uuid::new_v4().to_string();

    // Insert into DB.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let domain_with_path = if path_prefix.is_empty() {
            domain.clone()
        } else {
            format!("{domain}{path_prefix}")
        };
        db.execute(
            "INSERT INTO domains (id, domain, service_id, ssl_provider, path_prefix, compose_service, strip_prefix)
             VALUES (?1, ?2, ?3, 'letsencrypt', ?4, ?5, ?6)",
            rusqlite::params![
                id,
                domain_with_path,
                body.service_id,
                path_prefix,
                body.compose_service,
                body.strip_prefix as i64,
            ],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                AppError::Conflict(format!(
                    "Domain {domain}{path_prefix} is already registered"
                ))
            } else {
                AppError::Database(e)
            }
        })?;
    }

    // Re-emit the dynamic config covering EVERY domain of this service. Each
    // domain may target a different compose-service, hence the multi-target
    // builder.
    if let Err(e) = regenerate_for_service(&state, &body.service_id) {
        tracing::error!("Failed to write Traefik config for {domain}: {e}");
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute("DELETE FROM domains WHERE id = ?1", [&id]);
        return Err(AppError::Internal(anyhow::anyhow!(
            "Failed to configure proxy for {domain}: {e}"
        )));
    }

    // Traefik config written — SSL will be provisioned by Let's Encrypt (background monitor will update status)
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE domains SET ssl_status = 'provisioning' WHERE id = ?1",
            [&id],
        );
    }

    // Poke the SSL monitor shortly after Traefik picks up the new config so
    // `ssl_status` flips to `active` within seconds, not the next polling tick.
    {
        let notify = state.ssl_notify.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            notify.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            notify.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            notify.notify_one();
        });
    }

    let svc_tag = body
        .compose_service
        .as_deref()
        .map(|s| format!(" / {s}"))
        .unwrap_or_default();
    tracing::info!("Domain {domain} → service {service_name}{svc_tag} ({target_url})");

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "domain": domain,
    })))
}

/// DELETE /api/v1/domains/{id}
pub async fn remove(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    // Resolve the domain's service first so we can enforce on the right project.
    let service_id_for_check: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT service_id FROM domains WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .map_err(|_| AppError::NotFound(format!("Domain {id} not found")))?
    };
    enforce_resource_role(&state, &user, &service_id_for_check, ProjectRole::Editor)?;

    let service_id: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let sid: String = db
            .query_row(
                "SELECT service_id FROM domains WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .map_err(|_| AppError::NotFound(format!("Domain {id} not found")))?;
        db.execute("DELETE FROM domains WHERE id = ?1", [&id])?;
        sid
    };

    if let Err(e) = regenerate_for_service(&state, &service_id) {
        tracing::warn!("Failed to regenerate Traefik config for {service_id}: {e}");
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/v1/resources/{id}/domains — list domains for a specific service
pub async fn list_for_service(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(service_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &service_id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, domain, ssl_status, ssl_expires_at, ssl_provider, is_generated, created_at, compose_service, strip_prefix
         FROM domains WHERE service_id = ?1
         ORDER BY is_generated DESC, created_at ASC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&service_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "domain": row.get::<_, String>(1)?,
                "ssl_status": row.get::<_, String>(2)?,
                "ssl_expires_at": row.get::<_, Option<String>>(3)?,
                "ssl_provider": row.get::<_, String>(4)?,
                "is_generated": row.get::<_, i32>(5)? != 0,
                "created_at": row.get::<_, String>(6)?,
                "compose_service": row.get::<_, Option<String>>(7)?,
                "strip_prefix": row.get::<_, i32>(8)? != 0,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

// ──────────────────────────── helpers ────────────────────────────

/// Resolve the upstream URL for a service+compose_service pair.
///
/// Source of truth = the docker-compose YAML stored on the service. Both the
/// container hostname AND the container port are read straight from there:
/// `port_allocations` is a UI cache and must not gate routing decisions
/// (otherwise legacy rows with NULL `compose_service` block per-service
/// resolution).
///
/// Fallback (template / dockerfile services with no compose at all):
/// service's stored `container_id` + first non-management row in
/// `port_allocations`.
pub(crate) fn build_target_url(
    state: &AppState,
    service_id: &str,
    compose_service: Option<&str>,
) -> AppResult<String> {
    let (svc_name, container_id, compose_yaml): (String, Option<String>, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, container_id, compose_content FROM services WHERE id = ?1",
            [service_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Service {service_id} not found")))?
    };

    // Container hostname comes from the compose YAML's `container_name:` (which
    // strip_compose_ports preserves) when available. Falls back to the
    // detected services.container_id, then to a synthesized `pier-<name>`.
    let compose_svc_record = compose_yaml.as_deref().and_then(|yaml| {
        let env = crate::deploy::load_env_map(state, service_id);
        let parsed = crate::deploy::parse_compose_services(yaml, &env);
        match compose_service {
            Some(name) => parsed.into_iter().find(|s| s.name == name),
            None => parsed.into_iter().next(),
        }
    });

    // Resolution priority (must match the TCP path in proxy::sync_tcp_routes_for_service):
    //   1. Explicit `container_name:` from compose YAML — user intent.
    //   2. `services.container_id` from DB — post-deploy detection via
    //      `detect_container_name` (`docker compose ps`). This is the only
    //      source that knows Compose's actual `{project}-{service}-{replica}`
    //      naming and stays correct across Compose version changes.
    //   3. Synthesized `pier-{slug}` — last-resort fallback before the
    //      first deploy finishes (container_id is still NULL).
    let container_name = if let Some(svc) = compose_svc_record.as_ref() {
        if !svc.container_name.is_empty() {
            svc.container_name.clone()
        } else if let Some(cid) = container_id.as_deref().filter(|c| !c.is_empty()) {
            cid.to_string()
        } else {
            format!("pier-{}", svc.name.to_lowercase().replace(' ', "-"))
        }
    } else if let Some(cid) = container_id.as_deref().filter(|c| !c.is_empty()) {
        cid.to_string()
    } else if let Some(name) = compose_service {
        format!("pier-{}", name.to_lowercase().replace(' ', "-"))
    } else {
        format!("pier-{}", svc_name.to_lowercase().replace(' ', "-"))
    };

    // Port comes strictly from port_allocations — the same source of truth
    // that deploy uses for Traefik TCP routing. The compose YAML stored on
    // the service has its `ports:` blocks removed by strip_compose_ports
    // before persisting, so it cannot be the port source for the domain flow.
    //
    // Matching priority:
    //   1. Exact compose_service match.
    //   2. If no exact match and compose_service was requested: rows where
    //      compose_service IS NULL (legacy single-service composes store
    //      compose_service as NULL even when a service-name was requested).
    //   3. Otherwise (compose_service = None): all rows.
    // Within candidates, prefer non-management/metrics/prometheus ports.
    let http_keywords = ["management", "metrics", "prometheus"];
    let port: Option<u16> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT port_name, container_port, compose_service \
             FROM port_allocations WHERE service_id = ?1",
        )?;
        let rows: Vec<(String, u16, Option<String>)> = stmt
            .query_map([service_id], |row| {
                let port_name: String = row.get(0)?;
                let container_port: i64 = row.get(1)?;
                let cs: Option<String> = row.get(2)?;
                Ok((port_name, container_port as u16, cs))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let candidates: Vec<&(String, u16, Option<String>)> = match compose_service {
            Some(name) => {
                let exact: Vec<_> = rows
                    .iter()
                    .filter(|(_, _, cs)| cs.as_deref() == Some(name))
                    .collect();
                if !exact.is_empty() {
                    exact
                } else {
                    rows.iter().filter(|(_, _, cs)| cs.is_none()).collect()
                }
            }
            None => rows.iter().collect(),
        };

        candidates
            .iter()
            .find(|(name, _, _)| {
                !http_keywords
                    .iter()
                    .any(|k| name.to_lowercase().contains(k))
            })
            .or(candidates.first())
            .map(|(_, p, _)| *p)
    };

    let port = port.ok_or_else(|| {
        let label = compose_service.unwrap_or("(default)");
        AppError::BadRequest(format!(
            "Service {svc_name} has no port assigned for compose service '{label}'"
        ))
    })?;

    Ok(format!("http://{container_name}:{port}"))
}

/// Rebuild the Traefik dynamic config for a service from the current set of
/// domains in the DB. Each row's `compose_service` (NULL or string) determines
/// which upstream URL it gets routed to.
pub(crate) fn regenerate_for_service(state: &AppState, service_id: &str) -> AppResult<()> {
    let domain_rows: Vec<(String, String, Option<String>, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT domain, COALESCE(path_prefix, ''), compose_service, strip_prefix \
             FROM domains WHERE service_id = ?1",
        )?;
        let rows: Vec<(String, String, Option<String>, bool)> = stmt
            .query_map([service_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i32>(3)? != 0,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if domain_rows.is_empty() {
        config::regenerate_service_config_multi(&state.config.data_dir, service_id, &[])
            .map_err(|e| anyhow::anyhow!("Remove Traefik config: {e}"))?;
        return Ok(());
    }

    // Resolve target URL once per (compose_service) group to avoid repeating
    // the same SQL queries for every domain.
    let mut url_cache: std::collections::HashMap<Option<String>, String> =
        std::collections::HashMap::new();
    let mut targets: Vec<DomainTarget> = Vec::new();
    for (domain, path_prefix, compose_svc, strip_prefix) in &domain_rows {
        let key = compose_svc.clone();
        let url = if let Some(u) = url_cache.get(&key) {
            u.clone()
        } else {
            let u = build_target_url(state, service_id, key.as_deref())?;
            url_cache.insert(key.clone(), u.clone());
            u
        };
        // domain stored already includes path; if path_prefix is also a column,
        // the combined value is what's in `domain`. The legacy column kept the
        // path duplicated — prefer the `domain` value as-is.
        let _ = path_prefix; // intentionally unused — legacy column
        targets.push(DomainTarget {
            domain: domain.clone(),
            use_tls: true,
            compose_service: compose_svc.clone(),
            target_url: url,
            strip_prefix: *strip_prefix,
        });
    }

    config::regenerate_service_config_multi(&state.config.data_dir, service_id, &targets)
        .map_err(|e| anyhow::anyhow!("Write Traefik config: {e}"))?;
    Ok(())
}

/// Create an auto-generated sslip.io domain for a service. Called internally
/// when a resource is first deployed and the proxy is enabled.
pub async fn create_service_domain(
    state: &SharedState,
    service_id: &str,
    service_name: &str,
    port: i32,
) -> Result<String, AppError> {
    let proxy_enabled = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT value FROM settings WHERE key = 'proxy.enabled'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default()
            == "true"
    };
    if !proxy_enabled {
        return Ok(String::new());
    }

    let server_ip = get_server_ip(state).await?;
    let domain = config::generate_service_domain(service_name, service_id, &server_ip);
    let id = uuid::Uuid::new_v4().to_string();

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let existing: i32 = db
            .query_row(
                "SELECT COUNT(*) FROM domains WHERE service_id = ?1 AND is_generated = 1",
                [service_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if existing > 0 {
            let existing_domain: String = db.query_row(
                "SELECT domain FROM domains WHERE service_id = ?1 AND is_generated = 1",
                [service_id],
                |row| row.get(0),
            )?;
            return Ok(existing_domain);
        }
        db.execute(
            "INSERT INTO domains (id, domain, service_id, ssl_provider, is_generated)
             VALUES (?1, ?2, ?3, 'letsencrypt', 1)",
            rusqlite::params![id, domain, service_id],
        )?;
    }

    regenerate_for_service(state, service_id)?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE domains SET ssl_status = 'provisioning' WHERE id = ?1",
            [&id],
        );
    }

    tracing::info!("Auto-generated domain: {domain} → :{port}");
    Ok(domain)
}

async fn get_server_ip(state: &SharedState) -> Result<String, AppError> {
    let cached = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT value FROM settings WHERE key = 'server.public_ip'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
    };
    if let Some(ip) = cached {
        if !ip.is_empty() {
            return Ok(ip);
        }
    }
    let ip = config::detect_public_ip()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Detect IP: {e}")))?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('server.public_ip', ?1)",
        [&ip],
    )?;
    Ok(ip)
}
