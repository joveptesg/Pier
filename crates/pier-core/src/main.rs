mod api;
mod auth;
mod backup;
mod catalog;
mod config;
mod db;
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
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
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
            // Auto-detect and cache public IP
            match proxy::config::detect_public_ip().await {
                Ok(ip) => {
                    tracing::info!("Detected public IP: {ip}");
                    if let Ok(db) = proxy_state.db.lock() {
                        let _ = db.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('server.public_ip', ?1)",
                            [&ip],
                        );
                    }
                }
                Err(e) => tracing::warn!("Could not detect public IP: {e}"),
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
