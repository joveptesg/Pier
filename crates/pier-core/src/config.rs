use std::path::PathBuf;

/// TLS termination mode for the admin panel listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Auto-generated self-signed certificate, persisted under `tls_cert_dir`.
    SelfSigned,
    /// Plain HTTP. Only safe behind a TLS-terminating reverse proxy.
    Off,
}

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
    /// TLS termination mode for the panel listener.
    pub tls_mode: TlsMode,
    /// Directory holding the panel cert/key (`cert.pem`, `key.pem`).
    pub tls_cert_dir: PathBuf,
    /// Optional panel hostname, embedded as SAN in the self-signed cert.
    pub panel_domain: Option<String>,
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

        let tls_mode = match env_or("PIER_TLS_MODE", "self_signed")
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "none" | "disabled" => TlsMode::Off,
            _ => TlsMode::SelfSigned,
        };
        let tls_cert_dir = std::env::var("PIER_TLS_CERT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let dir: &PathBuf = &data_dir;
                dir.join("tls")
            });
        let panel_domain = std::env::var("PIER_PANEL_DOMAIN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        Self {
            // `::` binds both IPv4 and IPv6 on Linux by default
            // (IPV6_V6ONLY=0). An IPv6-only peer can't reach a primary
            // bound on `0.0.0.0`, so the safe default has to be the
            // dual-stack wildcard. Operators who explicitly set
            // `PIER_HOST=0.0.0.0` keep their v4-only listener
            // unchanged.
            host: env_or("PIER_HOST", "::"),
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
            tls_mode,
            tls_cert_dir,
            panel_domain,
        }
    }

    /// Listen address string for binding. Brackets IPv6 wildcards
    /// (`::` → `[::]:PORT`) and literals so `SocketAddr::parse` is
    /// happy. v4 and hostnames pass through unchanged.
    pub fn listen_addr(&self) -> String {
        let host = &self.host;
        let needs_brackets =
            host.contains("::") || (host.matches(':').count() >= 2 && !host.contains('.'));
        if needs_brackets && !host.starts_with('[') {
            format!("[{host}]:{}", self.port)
        } else {
            format!("{}:{}", host, self.port)
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
