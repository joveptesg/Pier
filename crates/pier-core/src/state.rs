use std::sync::{Arc, Mutex};

use bollard::Docker;
use minijinja::Environment;
use rusqlite::Connection;

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
}

/// Type alias for Arc-wrapped shared state.
pub type SharedState = Arc<AppState>;
