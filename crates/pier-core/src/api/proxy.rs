use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct ProxySettingsRequest {
    pub acme_email: Option<String>,
    pub dashboard: Option<bool>,
    pub wildcard_domain: Option<String>,
    pub platform_domain: Option<String>,
}

/// POST /api/v1/proxy/enable
pub async fn enable(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Get settings
    let (acme_email, dashboard) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let email = db
            .query_row(
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
                .unwrap_or_else(|_| "admin@pier.local".to_string())
            });
        let dash = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "false".to_string())
            == "true";
        (email, dash)
    };

    let version = read_traefik_version(&state)?;

    // Deploy Traefik
    crate::proxy::deploy_traefik(
        &state.docker,
        &state.config.data_dir,
        &acme_email,
        dashboard,
        &version,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy Traefik: {e}")))?;

    // Save enabled state
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.enabled', 'true')",
            [],
        )?;
    }

    Ok(Json(
        serde_json::json!({"ok": true, "message": "Proxy enabled"}),
    ))
}

/// POST /api/v1/proxy/disable
pub async fn disable(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    crate::proxy::stop_traefik(&state.docker)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Stop Traefik: {e}")))?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.enabled', 'false')",
        [],
    )?;

    Ok(Json(
        serde_json::json!({"ok": true, "message": "Proxy disabled"}),
    ))
}

/// GET /api/v1/proxy/status
pub async fn status(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let traefik = crate::proxy::traefik_status(&state.docker)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Traefik status: {e}")))?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let enabled = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.enabled'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "false".to_string())
        == "true";

    let acme_email = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.acme_email'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    let wildcard_domain = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.wildcard_domain'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    let platform_domain = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    let server_ip = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'server.public_ip'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    let domain_count: i32 = db
        .query_row("SELECT COUNT(*) FROM domains", [], |row| row.get(0))
        .unwrap_or(0);

    let active_certs: i32 = db
        .query_row(
            "SELECT COUNT(*) FROM domains WHERE ssl_status = 'active'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(Json(serde_json::json!({
        "enabled": enabled,
        "traefik": traefik,
        "acme_email": acme_email,
        "wildcard_domain": wildcard_domain,
        "platform_domain": platform_domain,
        "server_ip": server_ip,
        "domain_count": domain_count,
        "active_certs": active_certs,
    })))
}

/// PUT /api/v1/proxy/settings
pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<ProxySettingsRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    if let Some(email) = &body.acme_email {
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.acme_email', ?1)",
            [email],
        )?;
    }
    if let Some(dashboard) = body.dashboard {
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.dashboard', ?1)",
            [if dashboard { "true" } else { "false" }],
        )?;
    }
    if let Some(wildcard) = &body.wildcard_domain {
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.wildcard_domain', ?1)",
            [wildcard],
        )?;
    }

    // Handle platform domain
    if let Some(domain) = &body.platform_domain {
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.platform_domain', ?1)",
            [domain],
        )?;
        drop(db); // release lock before file I/O
        let domain = domain.trim().to_string();
        if domain.is_empty() {
            let _ = crate::proxy::config::remove_platform_domain_config(&state.config.data_dir);
        } else {
            let target = format!("http://host.docker.internal:{}", state.config.port);
            crate::proxy::config::write_platform_domain_config(
                &state.config.data_dir,
                &domain,
                &target,
            )
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Platform domain config: {e}")))?;
        }
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// Read the configured Traefik version, falling back to the baked-in default.
fn read_traefik_version(state: &SharedState) -> AppResult<String> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let v: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.traefik_version'",
            [],
            |row| row.get(0),
        )
        .ok()
        .filter(|v: &String| !v.is_empty())
        .unwrap_or_else(|| crate::proxy::DEFAULT_TRAEFIK_VERSION.to_string());
    Ok(v)
}

/// GET /api/v1/proxy/version — current Traefik tag + latest upstream release.
pub async fn version(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let current = read_traefik_version(&state)?;

    // Latest from GitHub Releases. Soft-fail: if network is down, just report current.
    let latest = fetch_latest_traefik_version().await.unwrap_or_else(|e| {
        tracing::debug!("Traefik latest fetch failed: {e}");
        current.clone()
    });

    let update_available = version_is_newer(&latest, &current);

    Ok(Json(serde_json::json!({
        "current": current,
        "latest": latest,
        "update_available": update_available,
    })))
}

async fn fetch_latest_traefik_version() -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("pier")
        .build()?;
    let resp: serde_json::Value = client
        .get("https://api.github.com/repos/traefik/traefik/releases/latest")
        .send()
        .await?
        .json()
        .await?;
    resp.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no tag_name in GitHub response"))
}

/// Rough semver comparison — both tags look like "v3.3" or "v3.4.1".
fn version_is_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    let av = parse(a);
    let bv = parse(b);
    for i in 0..av.len().max(bv.len()) {
        let x = *av.get(i).unwrap_or(&0);
        let y = *bv.get(i).unwrap_or(&0);
        if x > y {
            return true;
        }
        if x < y {
            return false;
        }
    }
    false
}

/// POST /api/v1/proxy/update — pull latest Traefik image and recreate container.
pub async fn update(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let latest = fetch_latest_traefik_version()
        .await
        .map_err(|e| AppError::BadRequest(format!("Could not fetch latest Traefik version: {e}")))?;

    // Persist the new version so auto-deploy paths use it on startup.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.traefik_version', ?1)",
            [&latest],
        )?;
    }

    // Re-read acme settings for the redeploy
    let (acme_email, dashboard) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let email = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.acme_email'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "admin@pier.local".to_string());
        let dash = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "false".to_string())
            == "true";
        (email, dash)
    };

    crate::proxy::deploy_traefik(
        &state.docker,
        &state.config.data_dir,
        &acme_email,
        dashboard,
        &latest,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("Deploy Traefik {latest}: {e}")))?;

    Ok(Json(serde_json::json!({"ok": true, "version": latest})))
}
