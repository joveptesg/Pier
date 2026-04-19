use std::sync::{Arc, Mutex};

use bollard::Docker;
use minijinja::Environment;
use rusqlite::Connection;
use tokio::sync::Notify;

use crate::catalog::CatalogItem;
use crate::config::PierConfig;

/// Shared application state passed to all Axum handlers.
pub struct AppState {
    /// SQLite connection (Mutex because rusqlite Connection is !Send).
    /// WAL mode allows concurrent reads at the engine level.
    pub db: Mutex<Connection>,

    /// Docker client via Bollard (thread-safe, cloneable).
    pub docker: Docker,

    /// MiniJinja template environment with all templates loaded.
    pub templates: Environment<'static>,

    /// Application configuration.
    pub config: PierConfig,

    /// Service catalog templates loaded from embedded TOML files.
    pub catalog: Vec<CatalogItem>,

    /// Process start time (for uptime calculation).
    pub started_at: std::time::Instant,

    /// Wakes the SSL monitor loop when a domain is added/removed so that
    /// `ssl_status` transitions from `provisioning` → `active` promptly,
    /// instead of waiting for the next polling tick.
    pub ssl_notify: Arc<Notify>,
}

/// Type alias for Arc-wrapped shared state.
pub type SharedState = Arc<AppState>;
