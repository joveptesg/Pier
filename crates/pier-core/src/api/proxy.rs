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
        let email = crate::proxy::read_acme_email(&db);
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

    // The platform domain router is "active" when its dynamic config file
    // exists AND embeds the currently configured (normalized) host. This is
    // what Traefik actually loads — useful as a quick UI sanity check.
    let platform_domain_active = !platform_domain.is_empty()
        && crate::proxy::config::platform_domain_router_present(
            &state.config.data_dir,
            &platform_domain,
        );

    Ok(Json(serde_json::json!({
        "enabled": enabled,
        "traefik": traefik,
        "acme_email": acme_email,
        "wildcard_domain": wildcard_domain,
        "platform_domain": platform_domain,
        "platform_domain_active": platform_domain_active,
        "server_ip": server_ip,
        "domain_count": domain_count,
        "active_certs": active_certs,
    })))
}

/// Reject contacts Let's Encrypt will not accept.
///
/// The failure this guards against is specific: an address on a reserved
/// suffix — `admin@pier.local` is Pier's own fallback — makes the ACME account
/// registration fail outright with "Domain name does not end with a valid
/// public suffix (TLD)", and Traefik then issues no certificates at all. The
/// only trace is a line in the Traefik container log, so the operator is left
/// with a panel that saved successfully and HTTPS that never works.
///
/// Deliberately not a full RFC 5322 validator: the goal is to catch the
/// address that cannot possibly work, not to argue about exotic-but-legal ones.
fn validate_acme_email(email: &str) -> Result<(), AppError> {
    let email = email.trim();
    if email.is_empty() {
        return Ok(()); // clearing the setting is allowed
    }
    let reject = |reason: &str| {
        Err(AppError::BadRequest(format!(
            "'{email}' cannot be used for Let's Encrypt: {reason}"
        )))
    };
    let Some((local, domain)) = email.split_once('@') else {
        return reject("not an email address");
    };
    if local.is_empty() || domain.is_empty() {
        return reject("not an email address");
    }
    if !domain.contains('.') {
        return reject("the domain has no public suffix");
    }
    const RESERVED: &[&str] = &[
        ".local",
        ".localdomain",
        ".internal",
        ".lan",
        ".home",
        ".test",
    ];
    let lower = domain.to_ascii_lowercase();
    if RESERVED.iter().any(|s| lower.ends_with(s)) {
        return reject("that suffix is reserved and Let's Encrypt rejects it");
    }
    Ok(())
}

/// PUT /api/v1/proxy/settings
pub async fn update_settings(
    State(state): State<SharedState>,
    Json(body): Json<ProxySettingsRequest>,
) -> AppResult<impl IntoResponse> {
    if let Some(email) = &body.acme_email {
        validate_acme_email(email)?;
    }

    // All DB writes happen in this scope so the guard is released before any
    // file I/O or Docker work below. `std::sync::Mutex` is not reentrant, and
    // deploying Traefik takes seconds — holding the lock across it would stall
    // every other handler.
    let platform_domain = body
        .platform_domain
        .as_deref()
        .map(crate::proxy::config::normalize_domain);

    let (acme_email, dashboard_enabled, acme_email_changed) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        // Compare before writing: re-saving the same address must not bounce
        // Traefik, and neither must saving an unrelated setting.
        let previous = crate::proxy::read_acme_email(&db);
        let changed = body
            .acme_email
            .as_deref()
            .map(str::trim)
            .is_some_and(|new| !new.is_empty() && new != previous);

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
        if let Some(domain) = &platform_domain {
            db.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.platform_domain', ?1)",
                [domain],
            )?;
        }

        let email = crate::proxy::read_acme_email(&db);
        let dash = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "false".to_string())
            == "true";
        (email, dash, changed)
    };

    // Handle platform domain (Traefik DYNAMIC config — a watched directory, so
    // this one does take effect without a restart).
    if let Some(domain) = &platform_domain {
        let domain = domain.clone();
        if domain.is_empty() {
            let _ = crate::proxy::config::remove_platform_domain_config(&state.config.data_dir);
            tracing::info!("Platform domain cleared (Traefik dynamic config removed)");
        } else {
            let (scheme, insecure) = match state.config.tls_mode {
                crate::config::TlsMode::SelfSigned => ("https", true),
                crate::config::TlsMode::Off => ("http", false),
            };
            let target = format!("{scheme}://host.docker.internal:{}", state.config.port);
            crate::proxy::config::write_platform_domain_config(
                &state.config.data_dir,
                &domain,
                &target,
                insecure,
            )
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Platform domain config: {e}")))?;
            tracing::info!(
                "Platform domain bound: {domain} -> {target} (Traefik dynamic config written)"
            );
        }
    }

    // The ACME contact lives in Traefik's STATIC config, which is only written
    // when Traefik is deployed — and Traefik does not re-read it (only the
    // dynamic directory is watched). Storing the address was therefore a no-op
    // until something else happened to redeploy: `proxy::enable`, a version
    // upgrade, or the next restart of Pier. The operator saw `{"ok": true}`,
    // certificates kept failing against whatever contact was baked in at first
    // deploy — on a fresh install the `admin@pier.local` fallback, which Let's
    // Encrypt refuses outright — and nothing said a restart was needed.
    // Only when the address actually changed, and never inline.
    //
    // Redeploying stops and restarts the Traefik container, and the operator is
    // almost always talking to the panel *through* Traefik — so doing it inside
    // the request kills the very connection carrying it. axum then drops the
    // handler future mid-redeploy and Traefik stays down: saving an unrelated
    // setting would take the whole panel offline. The task owns the work, so it
    // finishes whether or not the client survives the blip.
    if acme_email_changed {
        let version = read_traefik_version(&state)?;
        let state = state.clone();
        let email = acme_email.clone();
        tokio::spawn(async move {
            match crate::proxy::deploy_traefik(
                &state.docker,
                &state.config.data_dir,
                &email,
                dashboard_enabled,
                &version,
            )
            .await
            {
                Err(e) => tracing::warn!("ACME email saved but Traefik redeploy failed: {e}"),
                Ok(()) => tracing::info!("ACME contact set to {email}; Traefik redeployed"),
            }
        });
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
///
/// Resilient: if the new version fails to start (e.g. breaking change in a
/// Traefik release crashes on this server's config), automatically rolls back
/// to the previously running version so the platform stays online.
pub async fn update(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let latest = fetch_latest_traefik_version().await.map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.proxy.fetch_latest_version_failed",
            &[("v", &e.to_string())],
        ))
    })?;

    // Snapshot the currently running version, persist it as `previous` so we
    // can roll back if the new deploy fails. Then write the new version.
    let previous: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let current: String = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.traefik_version'",
                [],
                |row| row.get(0),
            )
            .ok()
            .filter(|v: &String| !v.is_empty())
            .unwrap_or_else(|| crate::proxy::DEFAULT_TRAEFIK_VERSION.to_string());
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.traefik_version_previous', ?1)",
            [&current],
        )?;
        db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.traefik_version', ?1)",
            [&latest],
        )?;
        current
    };

    // Re-read acme settings for the redeploy
    let (acme_email, dashboard) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let email = crate::proxy::read_acme_email(&db);
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

    match crate::proxy::deploy_traefik(
        &state.docker,
        &state.config.data_dir,
        &acme_email,
        dashboard,
        &latest,
    )
    .await
    {
        Ok(_) => Ok(Json(serde_json::json!({"ok": true, "version": latest}))),
        Err(deploy_err) => {
            tracing::error!(
                "Traefik {latest} failed to start: {deploy_err}; rolling back to {previous}"
            );

            // Roll back the version setting so subsequent restarts use the old image
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.traefik_version', ?1)",
                    [&previous],
                )?;
            }

            // Single rollback attempt — no retry loop
            match crate::proxy::deploy_traefik(
                &state.docker,
                &state.config.data_dir,
                &acme_email,
                dashboard,
                &previous,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("Rollback to Traefik {previous} succeeded");
                    Err(AppError::BadRequest(crate::i18n::te_args(
                        "errors.proxy.update_failed_rolled_back",
                        &[
                            ("latest", &latest),
                            ("err", &deploy_err.to_string()),
                            ("previous", &previous),
                        ],
                    )))
                }
                Err(rollback_err) => Err(AppError::Internal(anyhow::anyhow!(
                    "Update to Traefik {latest} failed: {deploy_err}. Rollback to {previous} ALSO failed: {rollback_err}. Manual recovery required."
                ))),
            }
        }
    }
}
