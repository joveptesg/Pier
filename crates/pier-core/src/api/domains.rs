use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::proxy::config;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateDomainRequest {
    pub domain: String,
    pub service_id: String,
}

/// GET /api/v1/domains
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT d.id, d.domain, d.service_id, d.ssl_status, d.ssl_expires_at,
                d.ssl_provider, d.is_generated, d.created_at, s.name as service_name
         FROM domains d
         LEFT JOIN services s ON d.service_id = s.id
         ORDER BY d.created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "domain": row.get::<_, String>(1)?,
                "service_id": row.get::<_, String>(2)?,
                "ssl_status": row.get::<_, String>(3)?,
                "ssl_expires_at": row.get::<_, Option<String>>(4)?,
                "ssl_provider": row.get::<_, String>(5)?,
                "is_generated": row.get::<_, i32>(6)? != 0,
                "created_at": row.get::<_, String>(7)?,
                "service_name": row.get::<_, Option<String>>(8)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/domains
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateDomainRequest>,
) -> AppResult<impl IntoResponse> {
    let domain = body.domain.trim().to_lowercase();
    if domain.is_empty() {
        return Err(AppError::BadRequest("Domain is required".into()));
    }

    // Look up the service to get its port
    let (service_name, port) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, port FROM services WHERE id = ?1",
            [&body.service_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i32>>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Service {} not found", body.service_id)))?
    };

    let port = port.ok_or_else(|| {
        AppError::BadRequest(format!("Service {service_name} has no port assigned"))
    })?;

    let id = uuid::Uuid::new_v4().to_string();
    let target_url = format!("http://127.0.0.1:{port}");

    // Insert into DB
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO domains (id, domain, service_id, ssl_provider)
             VALUES (?1, ?2, ?3, 'letsencrypt')",
            rusqlite::params![id, domain, body.service_id],
        )
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                AppError::Conflict(format!("Domain {domain} is already registered"))
            } else {
                AppError::Database(e)
            }
        })?;
    }

    // Regenerate Traefik config with ALL domains for this service
    let all_domains: Vec<(String, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare("SELECT domain FROM domains WHERE service_id = ?1")?;
        let rows = stmt
            .query_map([&body.service_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|d| (d, true))
            .collect();
        rows
    };

    if let Err(e) = config::regenerate_service_config(
        &state.config.data_dir,
        &body.service_id,
        &all_domains,
        &target_url,
    ) {
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

    // Traefik config written — SSL will be provisioned automatically by Let's Encrypt
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE domains SET ssl_status = 'active' WHERE id = ?1",
            [&id],
        );
    }

    tracing::info!("Domain {domain} → service {service_name} (:{port})");

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "domain": domain,
    })))
}

/// DELETE /api/v1/domains/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (service_id, port) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let row = db
            .query_row(
                "SELECT d.service_id, s.port FROM domains d
                 LEFT JOIN services s ON d.service_id = s.id
                 WHERE d.id = ?1",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i32>>(1)?)),
            )
            .map_err(|_| AppError::NotFound(format!("Domain {id} not found")))?;
        db.execute("DELETE FROM domains WHERE id = ?1", [&id])?;
        row
    };

    // Regenerate config with remaining domains
    let remaining: Vec<(String, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare("SELECT domain FROM domains WHERE service_id = ?1")?;
        let rows = stmt
            .query_map([&service_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|d| (d, true))
            .collect();
        rows
    };

    let target_url = format!("http://127.0.0.1:{}", port.unwrap_or(0));
    if let Err(e) = config::regenerate_service_config(
        &state.config.data_dir,
        &service_id,
        &remaining,
        &target_url,
    ) {
        tracing::warn!("Failed to regenerate Traefik config for {service_id}: {e}");
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/v1/resources/{id}/domains — list domains for a specific service
pub async fn list_for_service(
    State(state): State<SharedState>,
    Path(service_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, domain, ssl_status, ssl_expires_at, ssl_provider, is_generated, created_at
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
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// Create an auto-generated sslip.io domain for a service.
/// Called internally when a resource is deployed and proxy is enabled.
pub async fn create_service_domain(
    state: &SharedState,
    service_id: &str,
    service_name: &str,
    port: i32,
) -> Result<String, AppError> {
    // Check if proxy is enabled
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

    // Get server IP
    let server_ip = get_server_ip(state).await?;

    // Generate domain
    let domain = config::generate_service_domain(service_name, service_id, &server_ip);
    let target_url = format!("http://127.0.0.1:{port}");
    let id = uuid::Uuid::new_v4().to_string();

    // Insert into DB
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        // Skip if domain already exists for this service
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

    // Regenerate Traefik config with all domains for this service
    let all_domains: Vec<(String, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare("SELECT domain FROM domains WHERE service_id = ?1")?;
        let rows = stmt
            .query_map([service_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|d| (d, true))
            .collect();
        rows
    };
    config::regenerate_service_config(
        &state.config.data_dir,
        service_id,
        &all_domains,
        &target_url,
    )
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Write proxy config: {e}")))?;

    // Mark as active — Traefik handles SSL automatically
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE domains SET ssl_status = 'active' WHERE id = ?1",
            [&id],
        );
    }

    tracing::info!("Auto-generated domain: {domain} → :{port}");
    Ok(domain)
}

/// Get server public IP (cached in settings).
async fn get_server_ip(state: &SharedState) -> Result<String, AppError> {
    // Check cache
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

    // Detect and cache
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
