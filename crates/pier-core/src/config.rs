use std::path::PathBuf;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct PierConfig {
    /// HTTP listen address (default: "0.0.0.0")
    pub host: String,
    /// HTTP listen port (default: 8443)
    pub port: u16,
    /// Data directory for SQLite, compose files, etc.
    pub data_dir: PathBuf,
    /// SQLite database file path
    pub db_path: PathBuf,
    /// Docker socket path (None = auto-detect)
    pub docker_host: Option<String>,
    /// Session cookie name
    pub session_cookie: String,
    /// Session TTL in hours
    pub session_ttl_hours: u64,
    /// Log level
    pub log_level: String,
    /// Port range start for auto-allocation (default: 10000)
    pub port_range_start: u16,
    /// Port range end for auto-allocation (default: 65000)
    pub port_range_end: u16,
}

impl PierConfig {
    /// Load configuration from environment variables with sensible defaults.
    pub fn from_env() -> Self {
        let data_dir = env_or("PIER_DATA_DIR", "./data").into();
        let db_path = std::env::var("PIER_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let dir: &PathBuf = &data_dir;
                dir.join("pier.db")
            });

        Self {
            host: env_or("PIER_HOST", "0.0.0.0"),
            port: env_or("PIER_PORT", "8443").parse().unwrap_or(8443),
            data_dir,
            db_path,
            docker_host: std::env::var("PIER_DOCKER_HOST").ok(),
            session_cookie: env_or("PIER_SESSION_COOKIE", "pier_session"),
            session_ttl_hours: env_or("PIER_SESSION_TTL", "24").parse().unwrap_or(24),
            log_level: env_or("PIER_LOG_LEVEL", "info"),
            port_range_start: env_or("PIER_PORT_RANGE_START", "10000")
                .parse()
                .unwrap_or(10000),
            port_range_end: env_or("PIER_PORT_RANGE_END", "65000")
                .parse()
                .unwrap_or(65000),
        }
    }

    /// Listen address string for binding.
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
