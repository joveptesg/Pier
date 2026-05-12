use std::sync::{Arc, Mutex};

use bollard::Docker;
use minijinja::Environment;
use rusqlite::Connection;
use tokio::sync::Notify;

use crate::auth::partial_token::PartialTokenStore;
use crate::auth::setup_token::SetupTokenStore;
use crate::catalog::CatalogItem;
use crate::config::PierConfig;
use crate::docker::events::DockerEventBus;

/// Shared application state passed to all Axum handlers.
pub struct AppState {
    /// SQLite connection (Mutex because rusqlite Connection is !Send).
    /// WAL mode allows concurrent reads at the engine level.
    pub db: Mutex<Connection>,

    /// Docker client via Bollard (thread-safe, cloneable).
    pub docker: Docker,

    /// Docker events fan-out bus — one subscription per process, many
    /// broadcast receivers (WebSocket handlers, alert hooks, etc).
    pub event_bus: Arc<DockerEventBus>,

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

    /// One-shot bootstrap token guarding `/setup` on a fresh VPS. Loaded once
    /// at startup from `${PIER_DATA_DIR}/.setup_token`; cleared after the first
    /// admin is created.
    pub setup_token: Arc<SetupTokenStore>,

    /// Short-lived RAM-only tokens issued by the password step of login when
    /// the user has 2FA enabled. The TOTP step consumes them.
    pub partial_tokens: Arc<PartialTokenStore>,
}

/// Type alias for Arc-wrapped shared state.
pub type SharedState = Arc<AppState>;
