mod api;
mod auth;
mod backup;
mod catalog;
mod config;
mod db;
mod docker;
mod error;
mod git;
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
    match docker.ping().await {
        Ok(_) => tracing::info!("Docker connection OK"),
        Err(e) => tracing::warn!("Docker not available: {e} — container features will fail"),
    }

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
