use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub metric: String,
    pub scope: String,
    pub scope_id: Option<String>,
    pub threshold: Option<f64>,
    pub comparison: String,
    pub duration_secs: i64,
    pub severity: String,
    pub channel: String,
    pub channel_config_enc: String,
    pub cooldown_mins: i64,
    pub last_triggered_at: Option<String>,
    pub last_value: Option<f64>,
    pub last_state: String,
    pub first_breach_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Fire,
    Resolve,
    NoChange,
}

#[derive(Debug, Clone)]
pub struct AlertMessage {
    pub rule_name: String,
    pub severity: String,
    pub state: String,
    pub metric: String,
    pub scope_label: String,
    /// Human-readable host where the event happened, e.g. "X1 (178.18.249.144)".
    pub server_label: Option<String>,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub comparison: String,
    pub context: Option<String>,
    /// Pre-formatted timestamp in the configured system timezone, e.g. "2026-04-19 18:42 MSK".
    /// Populated in `build_message` so channel modules don't need state access.
    pub time_str: String,
}

pub fn severity_prefix(sev: &str) -> &'static str {
    match sev {
        "info" => "ℹ️",
        "critical" => "🚨",
        _ => "⚠️",
    }
}

pub fn format_metric_label(metric: &str) -> &'static str {
    match metric {
        "cpu" => "CPU",
        "ram" => "RAM",
        "disk" => "Disk",
        "agent_offline" => "Agent offline",
        "container_cpu" => "Container CPU",
        "container_ram" => "Container RAM",
        "container_status" => "Container status",
        "container_restarts" => "Container restarts",
        "ssl_expiry" => "SSL expiry",
        "deploy_status" => "Deploy failed",
        "deploy_success" => "Deploy succeeded",
        "backup_status" => "Backup failed",
        "backup_success" => "Backup succeeded",
        "docker_cleanup_success" => "Docker cleanup",
        "docker_cleanup_failure" => "Docker cleanup failed",
        "server_reachable" => "Server back online",
        _ => "Metric",
    }
}
