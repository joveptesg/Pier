mod api;
mod auth;
mod backup;
mod catalog;
mod config;
mod db;
mod deploy;
mod docker;
mod error;
mod git;
mod proxy;
mod s3;
mod state;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use bollard::Docker;
use tracing_subscriber::EnvFilter;

use config::PierConfig;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let config = PierConfig::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .init();

    tracing::info!("Pier v{}", env!("CARGO_PKG_VERSION"));

    // Initialize database
    let conn = db::init_db(&config.db_path)?;

    // Check if setup is needed
    let user_count = db::user_count(&conn)?;
    if user_count == 0 {
        tracing::info!("No users found — visit /setup to create admin account");
    } else {
        tracing::info!("{user_count} user(s) in database");
    }

    // Connect to Docker
    let docker = match &config.docker_host {
        Some(host) => Docker::connect_with_http(host, 120, bollard::API_DEFAULT_VERSION)?,
        None => Docker::connect_with_local_defaults()?,
    };

    // Verify Docker connection
    let docker_ok = match docker.ping().await {
        Ok(_) => {
            tracing::info!("Docker connection OK");
            true
        }
        Err(e) => {
            tracing::warn!("Docker not available: {e} — container features will fail");
            false
        }
    };

    // Read proxy settings before conn moves into Mutex
    let proxy_acme_email = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.acme_email'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            // Fallback: use the first admin user's email
            conn.query_row(
                "SELECT email FROM users WHERE role = 'admin' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "admin@pier.local".to_string())
        });
    let proxy_dashboard = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "false".to_string())
        == "true";
    let proxy_platform_domain = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();

    // Initialize templates
    let templates = ui::templates::init_templates();

    // Load service catalog
    let catalog = catalog::load_catalog();
    tracing::info!("Loaded {} catalog templates", catalog.len());

    // Build shared state
    let state = Arc::new(AppState {
        db: Mutex::new(conn),
        docker,
        templates,
        config: config.clone(),
        catalog,
    });

    // Start backup scheduler
    backup::scheduler::start_scheduler(state.clone());
    tracing::info!("Backup scheduler started");

    // Auto-start proxy (Traefik) in background
    if docker_ok {
        let proxy_state = state.clone();
        let proxy_data_dir = config.data_dir.clone();
        let proxy_port = config.port;
        tokio::spawn(async move {
            // Auto-detect and cache public IP + geolocation
            let public_ip = match proxy::config::detect_public_ip().await {
                Ok(ip) => {
                    tracing::info!("Detected public IP: {ip}");
                    if let Ok(db) = proxy_state.db.lock() {
                        let _ = db.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('server.public_ip', ?1)",
                            [&ip],
                        );
                    }
                    Some(ip)
                }
                Err(e) => {
                    tracing::warn!("Could not detect public IP: {e}");
                    None
                }
            };

            // Ensure local server record exists + detect geolocation
            if let Some(ref ip) = public_ip {
                if let Ok(db) = proxy_state.db.lock() {
                    // Deduplicate: keep only one local server
                    let local_count: i64 = db
                        .query_row(
                            "SELECT COUNT(*) FROM servers WHERE is_local = 1",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);

                    if local_count > 1 {
                        // Keep the first one, delete the rest
                        let first_id: String = db
                            .query_row(
                                "SELECT id FROM servers WHERE is_local = 1 ORDER BY created_at ASC LIMIT 1",
                                [],
                                |row| row.get(0),
                            )
                            .unwrap_or_else(|_| "local".to_string());
                        let _ = db.execute(
                            "DELETE FROM servers WHERE is_local = 1 AND id != ?1",
                            [&first_id],
                        );
                        tracing::info!("Removed duplicate local server records");
                    }

                    if local_count == 0 {
                        let hostname = sysinfo::System::host_name()
                            .unwrap_or_else(|| "localhost".to_string());
                        let _ = db.execute(
                            "INSERT INTO servers (id, name, host, port, agent_token, status, is_local, os_info)
                             VALUES ('local', ?1, ?2, 0, '', 'online', 1, ?3)",
                            rusqlite::params![
                                hostname,
                                ip,
                                format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                            ],
                        );
                        tracing::info!("Created local server record: {hostname}");
                    } else {
                        // Update host IP and system info
                        let _ = db.execute(
                            "UPDATE servers SET host = ?1, status = 'online', os_info = ?2, updated_at = datetime('now') WHERE is_local = 1",
                            rusqlite::params![ip, format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)],
                        );
                    }
                }

                // Detect geolocation via ip-api.com (free, no key required)
                match detect_geolocation(ip).await {
                    Ok((country, city, code)) => {
                        tracing::info!("Server location: {city}, {country} ({code})");
                        if let Ok(db) = proxy_state.db.lock() {
                            let _ = db.execute(
                                "UPDATE servers SET country = ?1, city = ?2, country_code = ?3 WHERE is_local = 1",
                                rusqlite::params![country, city, code],
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Geolocation detection failed: {e}"),
                }
            }

            // Deploy Traefik
            match proxy::deploy_traefik(
                &proxy_state.docker,
                &proxy_data_dir,
                &proxy_acme_email,
                proxy_dashboard,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("Proxy auto-started (Traefik)");
                    if let Ok(db) = proxy_state.db.lock() {
                        let _ = db.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.enabled', 'true')",
                            [],
                        );
                    }
                    // Write platform domain config if set
                    if !proxy_platform_domain.is_empty() {
                        let target = format!("http://host.docker.internal:{proxy_port}");
                        if let Err(e) = proxy::config::write_platform_domain_config(
                            &proxy_data_dir,
                            &proxy_platform_domain,
                            &target,
                        ) {
                            tracing::warn!("Failed to write platform domain config: {e}");
                        }
                    }
                }
                Err(e) => tracing::warn!("Proxy auto-start failed: {e}"),
            }
        });
    }

    // Build router: UI + API
    let app = ui::ui_router(state.clone())
        .merge(api::api_router(state.clone()))
        .with_state(state);

    // Start server
    let addr = config.listen_addr();
    tracing::info!("Listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Detect server geolocation from public IP using ip-api.com (free, no API key).
async fn detect_geolocation(ip: &str) -> anyhow::Result<(String, String, String)> {
    let url = format!("http://ip-api.com/json/{ip}?fields=country,city,countryCode");
    let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
    let country = resp["country"]
        .as_str()
        .unwrap_or("Unknown")
        .to_string();
    let city = resp["city"].as_str().unwrap_or("Unknown").to_string();
    let code = resp["countryCode"]
        .as_str()
        .unwrap_or("XX")
        .to_string();
    Ok((country, city, code))
}
