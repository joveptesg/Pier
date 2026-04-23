use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::alerts::types::{AlertMessage, AlertRule};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateRuleRequest {
    pub name: String,
    pub metric: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    pub scope_id: Option<String>,
    pub threshold: Option<f64>,
    #[serde(default = "default_comparison")]
    pub comparison: String,
    #[serde(default = "default_duration")]
    pub duration_secs: i64,
    #[serde(default = "default_severity")]
    pub severity: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    pub channel_config: Value,
    #[serde(default = "default_cooldown")]
    pub cooldown_mins: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_scope() -> String {
    "global".to_string()
}
fn default_comparison() -> String {
    "gt".to_string()
}
fn default_duration() -> i64 {
    60
}
fn default_severity() -> String {
    "warning".to_string()
}
fn default_channel() -> String {
    "telegram".to_string()
}
fn default_cooldown() -> i64 {
    30
}
fn default_true() -> bool {
    true
}

/// GET /api/v1/alerts — list alert rules (config masked).
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;

    let mut stmt = db.prepare(
        "SELECT id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                severity, channel, cooldown_mins, last_triggered_at, last_value, last_state,
                created_at, updated_at
         FROM alert_rules ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "name": row.get::<_, String>(1)?,
            "enabled": row.get::<_, i64>(2)? == 1,
            "metric": row.get::<_, String>(3)?,
            "scope": row.get::<_, String>(4)?,
            "scope_id": row.get::<_, Option<String>>(5)?,
            "threshold": row.get::<_, Option<f64>>(6)?,
            "comparison": row.get::<_, String>(7)?,
            "duration_secs": row.get::<_, i64>(8)?,
            "severity": row.get::<_, String>(9)?,
            "channel": row.get::<_, String>(10)?,
            "cooldown_mins": row.get::<_, i64>(11)?,
            "last_triggered_at": row.get::<_, Option<String>>(12)?,
            "last_value": row.get::<_, Option<f64>>(13)?,
            "last_state": row.get::<_, String>(14)?,
            "created_at": row.get::<_, String>(15)?,
            "updated_at": row.get::<_, String>(16)?,
        }))
    })?;

    let result: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
    Ok(Json(json!(result)))
}

/// POST /api/v1/alerts — create rule.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateRuleRequest>,
) -> AppResult<impl IntoResponse> {
    validate_metric(&body.metric)?;
    validate_comparison(&body.comparison)?;
    validate_severity(&body.severity)?;
    validate_channel(&body.channel)?;

    let id = uuid::Uuid::new_v4().to_string();
    let config_plain = serde_json::to_string(&body.channel_config)
        .map_err(|e| AppError::BadRequest(format!("channel_config: {e}")))?;
    let key = crate::crypto::get_secret_key();
    let config_enc = crate::crypto::encrypt(&config_plain, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encrypt: {e}")))?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "INSERT INTO alert_rules
               (id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                severity, channel, channel_config_enc, cooldown_mins)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                id,
                body.name,
                body.enabled as i64,
                body.metric,
                body.scope,
                body.scope_id,
                body.threshold,
                body.comparison,
                body.duration_secs,
                body.severity,
                body.channel,
                config_enc,
                body.cooldown_mins,
            ],
        )?;
    }
    Ok(Json(json!({"ok": true, "id": id})))
}

/// GET /api/v1/alerts/{id} — single rule (config masked).
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let row: Value = db
        .query_row(
            "SELECT id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                    severity, channel, cooldown_mins, last_triggered_at, last_value, last_state,
                    created_at, updated_at
             FROM alert_rules WHERE id = ?1",
            [&id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "enabled": row.get::<_, i64>(2)? == 1,
                    "metric": row.get::<_, String>(3)?,
                    "scope": row.get::<_, String>(4)?,
                    "scope_id": row.get::<_, Option<String>>(5)?,
                    "threshold": row.get::<_, Option<f64>>(6)?,
                    "comparison": row.get::<_, String>(7)?,
                    "duration_secs": row.get::<_, i64>(8)?,
                    "severity": row.get::<_, String>(9)?,
                    "channel": row.get::<_, String>(10)?,
                    "cooldown_mins": row.get::<_, i64>(11)?,
                    "last_triggered_at": row.get::<_, Option<String>>(12)?,
                    "last_value": row.get::<_, Option<f64>>(13)?,
                    "last_state": row.get::<_, String>(14)?,
                    "created_at": row.get::<_, String>(15)?,
                    "updated_at": row.get::<_, String>(16)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Alert {id} not found")))?;
    Ok(Json(row))
}

#[derive(Deserialize)]
pub struct UpdateRuleRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub metric: Option<String>,
    pub scope: Option<String>,
    pub scope_id: Option<Option<String>>,
    pub threshold: Option<Option<f64>>,
    pub comparison: Option<String>,
    pub duration_secs: Option<i64>,
    pub severity: Option<String>,
    pub channel: Option<String>,
    pub channel_config: Option<Value>,
    pub cooldown_mins: Option<i64>,
}

/// PUT /api/v1/alerts/{id} — update rule (partial).
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRuleRequest>,
) -> AppResult<impl IntoResponse> {
    if let Some(m) = &body.metric {
        validate_metric(m)?;
    }
    if let Some(c) = &body.comparison {
        validate_comparison(c)?;
    }
    if let Some(s) = &body.severity {
        validate_severity(s)?;
    }
    if let Some(c) = &body.channel {
        validate_channel(c)?;
    }

    let config_enc = if let Some(cfg) = &body.channel_config {
        let plain = serde_json::to_string(cfg)
            .map_err(|e| AppError::BadRequest(format!("channel_config: {e}")))?;
        let key = crate::crypto::get_secret_key();
        Some(
            crate::crypto::encrypt(&plain, &key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Encrypt: {e}")))?,
        )
    } else {
        None
    };

    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;

    macro_rules! set {
        ($col:expr, $val:expr) => {
            if let Some(v) = $val {
                db.execute(
                    &format!("UPDATE alert_rules SET {} = ?1, updated_at = datetime('now') WHERE id = ?2", $col),
                    rusqlite::params![v, id],
                )?;
            }
        };
    }

    set!("name", body.name);
    if let Some(v) = body.enabled {
        db.execute(
            "UPDATE alert_rules SET enabled = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v as i64, id],
        )?;
    }
    set!("metric", body.metric);
    set!("scope", body.scope);
    if let Some(v) = body.scope_id {
        db.execute(
            "UPDATE alert_rules SET scope_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, id],
        )?;
    }
    if let Some(v) = body.threshold {
        db.execute(
            "UPDATE alert_rules SET threshold = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, id],
        )?;
    }
    set!("comparison", body.comparison);
    set!("duration_secs", body.duration_secs);
    set!("severity", body.severity);
    set!("channel", body.channel);
    set!("channel_config_enc", config_enc);
    set!("cooldown_mins", body.cooldown_mins);

    Ok(Json(json!({"ok": true})))
}

/// DELETE /api/v1/alerts/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    db.execute("DELETE FROM alert_rules WHERE id = ?1", [&id])?;
    Ok(Json(json!({"ok": true})))
}

/// POST /api/v1/alerts/{id}/toggle
pub async fn toggle(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    db.execute(
        "UPDATE alert_rules SET enabled = 1 - enabled, updated_at = datetime('now') WHERE id = ?1",
        [&id],
    )?;
    let enabled: i64 = db.query_row(
        "SELECT enabled FROM alert_rules WHERE id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    Ok(Json(json!({"ok": true, "enabled": enabled == 1})))
}

/// POST /api/v1/alerts/{id}/test — send a test notification via the configured channel.
pub async fn test(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let rule = {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.query_row(
            "SELECT name, severity, metric, scope, scope_id, channel, channel_config_enc,
                    threshold, comparison
             FROM alert_rules WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<f64>>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Alert {id} not found")))?
    };

    let (name, severity, metric, scope, scope_id, channel, config_enc, threshold, comparison) =
        rule;
    let scope_label = crate::alerts::metrics::scope_label(&state, &scope, scope_id.as_deref());

    let key = crate::crypto::get_secret_key();
    let config_json = crate::crypto::decrypt(&config_enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Decrypt: {e}")))?;

    let server_label =
        crate::alerts::metrics::resolve_server_label(&state, &scope, scope_id.as_deref());
    let msg = AlertMessage {
        rule_name: format!("[TEST] {name}"),
        severity,
        state: "firing".to_string(),
        metric,
        scope_label,
        server_label,
        value: threshold,
        threshold,
        comparison,
        context: Some("This is a test notification from Pier.".to_string()),
        time_str: crate::timezone::format_now(&state),
    };

    crate::alerts::channels::send(&channel, &config_json, &msg)
        .await
        .map_err(|e| AppError::BadRequest(format!("Delivery failed: {e}")))?;

    Ok(Json(json!({"ok": true})))
}

/// GET /api/v1/alerts/{id}/events
pub async fn rule_events(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let mut stmt = db.prepare(
        "SELECT id, state, value, message, delivered, delivery_error, created_at
         FROM alert_events WHERE rule_id = ?1 ORDER BY created_at DESC LIMIT 100",
    )?;
    let rows = stmt.query_map([&id], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "state": row.get::<_, String>(1)?,
            "value": row.get::<_, Option<f64>>(2)?,
            "message": row.get::<_, String>(3)?,
            "delivered": row.get::<_, i64>(4)? == 1,
            "delivery_error": row.get::<_, Option<String>>(5)?,
            "created_at": row.get::<_, String>(6)?,
        }))
    })?;
    let events: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
    Ok(Json(json!(events)))
}

/// GET /api/v1/alerts/events — global feed of latest events.
pub async fn events_feed(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let mut stmt = db.prepare(
        "SELECT e.id, e.rule_id, r.name, e.state, e.value, e.message, e.delivered,
                e.delivery_error, e.created_at
         FROM alert_events e
         LEFT JOIN alert_rules r ON r.id = e.rule_id
         ORDER BY e.created_at DESC LIMIT 200",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "rule_id": row.get::<_, String>(1)?,
            "rule_name": row.get::<_, Option<String>>(2)?,
            "state": row.get::<_, String>(3)?,
            "value": row.get::<_, Option<f64>>(4)?,
            "message": row.get::<_, String>(5)?,
            "delivered": row.get::<_, i64>(6)? == 1,
            "delivery_error": row.get::<_, Option<String>>(7)?,
            "created_at": row.get::<_, String>(8)?,
        }))
    })?;
    let events: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
    Ok(Json(json!(events)))
}

// --- Validators ---

fn validate_metric(m: &str) -> AppResult<()> {
    const ALLOWED: &[&str] = &[
        "cpu",
        "ram",
        "disk",
        "agent_offline",
        "container_cpu",
        "container_ram",
        "container_status",
        "container_restarts",
        "ssl_expiry",
        "deploy_status",
        "deploy_success",
        "backup_status",
        "backup_success",
        "docker_cleanup_success",
        "docker_cleanup_failure",
        "server_reachable",
    ];
    if ALLOWED.contains(&m) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Invalid metric: {m}")))
    }
}

fn validate_comparison(c: &str) -> AppResult<()> {
    if matches!(c, "gt" | "lt" | "eq") {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Invalid comparison: {c}")))
    }
}

fn validate_severity(s: &str) -> AppResult<()> {
    if matches!(s, "info" | "warning" | "critical") {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Invalid severity: {s}")))
    }
}

fn validate_channel(c: &str) -> AppResult<()> {
    if c == "telegram" {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Unsupported channel: {c}")))
    }
}

// Re-export AlertRule for use in other modules (silence dead_code if unused).
#[allow(dead_code)]
type _AlertRuleRef = AlertRule;

// ──────────────────────────────────────────────────────────────────────────
// /api/v1/notifications/* — simplified Coolify-style UI layer.
// One global channel config per channel type, plus toggle-able preset rules.
// ──────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UpdateTelegramRequest {
    pub enabled: Option<bool>,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
}

#[derive(serde::Serialize, Deserialize, Default)]
struct TelegramChannelConfig {
    #[serde(default)]
    bot_token: String,
    #[serde(default)]
    chat_id: String,
}

/// GET /api/v1/notifications/channels/telegram
pub async fn channel_get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let (enabled, config_enc) = read_channel(&state, "telegram")?;
    let has_config = !config_enc.is_empty();
    let mut chat_id_hint = String::new();
    if has_config {
        let key = crate::crypto::get_secret_key();
        if let Ok(plain) = crate::crypto::decrypt(&config_enc, &key) {
            if let Ok(cfg) = serde_json::from_str::<TelegramChannelConfig>(&plain) {
                chat_id_hint = cfg.chat_id;
            }
        }
    }
    Ok(Json(json!({
        "channel": "telegram",
        "enabled": enabled,
        "has_config": has_config,
        "chat_id": chat_id_hint,
    })))
}

/// PUT /api/v1/notifications/channels/telegram
pub async fn channel_put(
    State(state): State<SharedState>,
    Json(body): Json<UpdateTelegramRequest>,
) -> AppResult<impl IntoResponse> {
    let (mut enabled, current_enc) = read_channel(&state, "telegram")?;

    let key = crate::crypto::get_secret_key();
    let mut cfg: TelegramChannelConfig = if current_enc.is_empty() {
        TelegramChannelConfig::default()
    } else {
        crate::crypto::decrypt(&current_enc, &key)
            .ok()
            .and_then(|p| serde_json::from_str(&p).ok())
            .unwrap_or_default()
    };

    if let Some(t) = body.bot_token.as_ref() {
        if !t.is_empty() {
            cfg.bot_token = t.clone();
        }
    }
    if let Some(c) = body.chat_id.as_ref() {
        cfg.chat_id = c.clone();
    }

    if let Some(v) = body.enabled {
        if v && (cfg.bot_token.is_empty() || cfg.chat_id.is_empty()) {
            return Err(AppError::BadRequest(
                "bot_token and chat_id are required before enabling Telegram".into(),
            ));
        }
        enabled = v;
    }

    let plain = serde_json::to_string(&cfg)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize cfg: {e}")))?;
    let config_enc = if cfg.bot_token.is_empty() && cfg.chat_id.is_empty() {
        String::new()
    } else {
        crate::crypto::encrypt(&plain, &key)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt: {e}")))?
    };

    {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "UPDATE notification_channels SET enabled = ?1, config_enc = ?2, updated_at = datetime('now') WHERE channel = ?3",
            rusqlite::params![enabled as i64, config_enc, "telegram"],
        )?;
    }

    Ok(Json(json!({"ok": true, "enabled": enabled})))
}

/// POST /api/v1/notifications/channels/telegram/test
pub async fn channel_test(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let (_enabled, config_enc) = read_channel(&state, "telegram")?;
    if config_enc.is_empty() {
        return Err(AppError::BadRequest("Telegram is not configured".into()));
    }
    let key = crate::crypto::get_secret_key();
    let plain = crate::crypto::decrypt(&config_enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt: {e}")))?;

    let msg = AlertMessage {
        rule_name: "[TEST] Pier notifications".to_string(),
        severity: "info".to_string(),
        state: "firing".to_string(),
        metric: "deploy_status".to_string(),
        scope_label: "global".to_string(),
        server_label: crate::alerts::metrics::resolve_server_label(&state, "global", None),
        value: None,
        threshold: None,
        comparison: "eq".to_string(),
        context: Some("If you see this, Telegram notifications work.".to_string()),
        time_str: crate::timezone::format_now(&state),
    };

    crate::alerts::channels::send("telegram", &plain, &msg)
        .await
        .map_err(|e| AppError::BadRequest(format!("Delivery failed: {e}")))?;

    Ok(Json(json!({"ok": true})))
}

/// GET /api/v1/notifications/alerts — list preset rules with fields needed for the UI.
pub async fn preset_list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;

    let mut stmt = db.prepare(
        "SELECT id, name, enabled, metric, threshold, comparison, duration_secs,
                severity, last_triggered_at, last_state
         FROM alert_rules WHERE id LIKE 'preset-%' ORDER BY severity DESC, name ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "name": row.get::<_, String>(1)?,
            "enabled": row.get::<_, i64>(2)? == 1,
            "metric": row.get::<_, String>(3)?,
            "threshold": row.get::<_, Option<f64>>(4)?,
            "comparison": row.get::<_, String>(5)?,
            "duration_secs": row.get::<_, i64>(6)?,
            "severity": row.get::<_, String>(7)?,
            "last_triggered_at": row.get::<_, Option<String>>(8)?,
            "last_state": row.get::<_, String>(9)?,
        }))
    })?;
    let list: Vec<Value> = rows.filter_map(|r| r.ok()).collect();
    Ok(Json(json!(list)))
}

// ──────────────────────────────────────────────────────────────────────────
// Email channel endpoints
// ──────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UpdateEmailRequest {
    pub enabled: Option<bool>,
    pub driver: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    pub to_address: Option<String>,
    pub smtp: Option<SmtpPatch>,
    pub brevo: Option<ApiKeyPatch>,
    pub resend: Option<ApiKeyPatch>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SmtpPatch {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub encryption: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ApiKeyPatch {
    pub api_key: Option<String>,
}

fn load_email_config(
    state: &SharedState,
) -> AppResult<crate::alerts::channels::email::EmailConfig> {
    let (_enabled, enc) = read_channel(state, "email")?;
    if enc.is_empty() {
        return Ok(Default::default());
    }
    let key = crate::crypto::get_secret_key();
    let plain = crate::crypto::decrypt(&enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt: {e}")))?;
    serde_json::from_str(&plain)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("parse email cfg: {e}")))
}

/// GET /api/v1/notifications/channels/email — status + masked fields.
pub async fn channel_email_get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let (enabled, enc) = read_channel(&state, "email")?;
    let has_config = !enc.is_empty();
    let cfg = load_email_config(&state).unwrap_or_default();
    Ok(Json(json!({
        "channel": "email",
        "enabled": enabled,
        "has_config": has_config,
        "driver": if cfg.driver.is_empty() { "smtp".to_string() } else { cfg.driver },
        "from_name": cfg.from_name,
        "from_address": cfg.from_address,
        "to_address": cfg.to_address,
        "smtp": {
            "host": cfg.smtp.host,
            "port": if cfg.smtp.port == 0 { 587 } else { cfg.smtp.port },
            "encryption": if cfg.smtp.encryption.is_empty() { "starttls".to_string() } else { cfg.smtp.encryption },
            "username": cfg.smtp.username,
            "has_password": !cfg.smtp.password.is_empty(),
            "timeout": if cfg.smtp.timeout == 0 { 30 } else { cfg.smtp.timeout },
        },
        "brevo":  { "has_api_key": !cfg.brevo.api_key.is_empty() },
        "resend": { "has_api_key": !cfg.resend.api_key.is_empty() },
    })))
}

/// PUT /api/v1/notifications/channels/email — merge fields, enable/disable.
pub async fn channel_email_put(
    State(state): State<SharedState>,
    Json(body): Json<UpdateEmailRequest>,
) -> AppResult<impl IntoResponse> {
    let mut cfg = load_email_config(&state).unwrap_or_default();
    let (mut enabled, _) = read_channel(&state, "email")?;

    if let Some(d) = body.driver.as_ref() {
        if !matches!(d.as_str(), "smtp" | "brevo" | "resend") {
            return Err(AppError::BadRequest(format!("Unknown driver: {d}")));
        }
        cfg.driver = d.clone();
    }
    if cfg.driver.is_empty() {
        cfg.driver = "smtp".into();
    }
    if let Some(v) = body.from_name {
        cfg.from_name = v;
    }
    if let Some(v) = body.from_address {
        cfg.from_address = v;
    }
    if let Some(v) = body.to_address {
        cfg.to_address = v;
    }
    if let Some(s) = body.smtp {
        if let Some(v) = s.host {
            cfg.smtp.host = v;
        }
        if let Some(v) = s.port {
            cfg.smtp.port = v;
        }
        if let Some(v) = s.encryption {
            cfg.smtp.encryption = v;
        }
        if let Some(v) = s.username {
            cfg.smtp.username = v;
        }
        if let Some(v) = s.password {
            if !v.is_empty() {
                cfg.smtp.password = v;
            }
        }
        if let Some(v) = s.timeout {
            cfg.smtp.timeout = v;
        }
    }
    if let Some(b) = body.brevo {
        if let Some(v) = b.api_key {
            if !v.is_empty() {
                cfg.brevo.api_key = v;
            }
        }
    }
    if let Some(r) = body.resend {
        if let Some(v) = r.api_key {
            if !v.is_empty() {
                cfg.resend.api_key = v;
            }
        }
    }

    // Validate when enabling
    if let Some(v) = body.enabled {
        if v {
            let ready = match cfg.driver.as_str() {
                "smtp" => !cfg.smtp.host.is_empty(),
                "brevo" => !cfg.brevo.api_key.is_empty(),
                "resend" => !cfg.resend.api_key.is_empty(),
                _ => false,
            };
            if !ready || cfg.from_address.is_empty() || cfg.to_address.is_empty() {
                return Err(AppError::BadRequest(
                    "Fill driver credentials and from/to addresses before enabling".into(),
                ));
            }
        }
        enabled = v;
    }

    let plain = serde_json::to_string(&cfg)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize: {e}")))?;
    let key = crate::crypto::get_secret_key();
    let config_enc = crate::crypto::encrypt(&plain, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt: {e}")))?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "UPDATE notification_channels SET enabled = ?1, config_enc = ?2, updated_at = datetime('now') WHERE channel = ?3",
            rusqlite::params![enabled as i64, config_enc, "email"],
        )?;
    }

    Ok(Json(json!({"ok": true, "enabled": enabled})))
}

/// POST /api/v1/notifications/channels/email/test — send a test email via active driver.
pub async fn channel_email_test(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let cfg = load_email_config(&state)?;
    if cfg.from_address.is_empty() || cfg.to_address.is_empty() {
        return Err(AppError::BadRequest(
            "from_address and to_address are required".into(),
        ));
    }
    let msg = AlertMessage {
        rule_name: "[TEST] Pier notifications".to_string(),
        severity: "info".to_string(),
        state: "firing".to_string(),
        metric: "deploy_status".to_string(),
        scope_label: "global".to_string(),
        server_label: crate::alerts::metrics::resolve_server_label(&state, "global", None),
        value: None,
        threshold: None,
        comparison: "eq".to_string(),
        context: Some("If you see this, email notifications work.".to_string()),
        time_str: crate::timezone::format_now(&state),
    };
    crate::alerts::channels::email::send(&cfg, &msg)
        .await
        .map_err(|e| AppError::BadRequest(format!("Delivery failed: {e}")))?;
    Ok(Json(json!({"ok": true})))
}

// ──────────────────────────────────────────────────────────────────────────
// Webhook-based channels — Discord + Slack share the same config shape
// (webhook URL only; Discord adds an opt-in @here ping).
// ──────────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UpdateDiscordRequest {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
    pub ping: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct UpdateSlackRequest {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
}

fn load_webhook_config<T>(state: &SharedState, channel: &str) -> AppResult<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    let (_enabled, enc) = read_channel(state, channel)?;
    if enc.is_empty() {
        return Ok(T::default());
    }
    let key = crate::crypto::get_secret_key();
    let plain = crate::crypto::decrypt(&enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt: {e}")))?;
    serde_json::from_str(&plain).map_err(|e| AppError::Internal(anyhow::anyhow!("parse cfg: {e}")))
}

/// GET /api/v1/notifications/channels/discord
pub async fn channel_discord_get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let (enabled, enc) = read_channel(&state, "discord")?;
    let cfg: crate::alerts::channels::discord::DiscordConfig =
        load_webhook_config(&state, "discord").unwrap_or_default();
    Ok(Json(json!({
        "channel": "discord",
        "enabled": enabled,
        "has_config": !enc.is_empty(),
        "webhook_url_masked": mask_url(&cfg.webhook_url),
        "ping": cfg.ping,
    })))
}

/// PUT /api/v1/notifications/channels/discord
pub async fn channel_discord_put(
    State(state): State<SharedState>,
    Json(body): Json<UpdateDiscordRequest>,
) -> AppResult<impl IntoResponse> {
    let mut cfg: crate::alerts::channels::discord::DiscordConfig =
        load_webhook_config(&state, "discord").unwrap_or_default();
    let (mut enabled, _) = read_channel(&state, "discord")?;

    if let Some(u) = body.webhook_url {
        if !u.is_empty() {
            cfg.webhook_url = u;
        }
    }
    if let Some(p) = body.ping {
        cfg.ping = p;
    }
    if let Some(v) = body.enabled {
        if v && cfg.webhook_url.is_empty() {
            return Err(AppError::BadRequest(
                "webhook_url is required before enabling Discord".into(),
            ));
        }
        enabled = v;
    }

    save_channel(&state, "discord", enabled, &cfg)?;
    Ok(Json(json!({"ok": true, "enabled": enabled})))
}

/// POST /api/v1/notifications/channels/discord/test
pub async fn channel_discord_test(
    State(state): State<SharedState>,
) -> AppResult<impl IntoResponse> {
    let cfg: crate::alerts::channels::discord::DiscordConfig =
        load_webhook_config(&state, "discord")?;
    if cfg.webhook_url.is_empty() {
        return Err(AppError::BadRequest("Discord is not configured".into()));
    }
    let msg = test_message(&state, "Discord");
    crate::alerts::channels::discord::send(&cfg, &msg)
        .await
        .map_err(|e| AppError::BadRequest(format!("Delivery failed: {e}")))?;
    Ok(Json(json!({"ok": true})))
}

/// GET /api/v1/notifications/channels/slack
pub async fn channel_slack_get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let (enabled, enc) = read_channel(&state, "slack")?;
    let cfg: crate::alerts::channels::slack::SlackConfig =
        load_webhook_config(&state, "slack").unwrap_or_default();
    Ok(Json(json!({
        "channel": "slack",
        "enabled": enabled,
        "has_config": !enc.is_empty(),
        "webhook_url_masked": mask_url(&cfg.webhook_url),
    })))
}

/// PUT /api/v1/notifications/channels/slack
pub async fn channel_slack_put(
    State(state): State<SharedState>,
    Json(body): Json<UpdateSlackRequest>,
) -> AppResult<impl IntoResponse> {
    let mut cfg: crate::alerts::channels::slack::SlackConfig =
        load_webhook_config(&state, "slack").unwrap_or_default();
    let (mut enabled, _) = read_channel(&state, "slack")?;

    if let Some(u) = body.webhook_url {
        if !u.is_empty() {
            cfg.webhook_url = u;
        }
    }
    if let Some(v) = body.enabled {
        if v && cfg.webhook_url.is_empty() {
            return Err(AppError::BadRequest(
                "webhook_url is required before enabling Slack".into(),
            ));
        }
        enabled = v;
    }

    save_channel(&state, "slack", enabled, &cfg)?;
    Ok(Json(json!({"ok": true, "enabled": enabled})))
}

/// POST /api/v1/notifications/channels/slack/test
pub async fn channel_slack_test(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let cfg: crate::alerts::channels::slack::SlackConfig = load_webhook_config(&state, "slack")?;
    if cfg.webhook_url.is_empty() {
        return Err(AppError::BadRequest("Slack is not configured".into()));
    }
    let msg = test_message(&state, "Slack");
    crate::alerts::channels::slack::send(&cfg, &msg)
        .await
        .map_err(|e| AppError::BadRequest(format!("Delivery failed: {e}")))?;
    Ok(Json(json!({"ok": true})))
}

fn save_channel<T: serde::Serialize>(
    state: &SharedState,
    channel: &str,
    enabled: bool,
    cfg: &T,
) -> AppResult<()> {
    let plain = serde_json::to_string(cfg)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize: {e}")))?;
    let key = crate::crypto::get_secret_key();
    let config_enc = crate::crypto::encrypt(&plain, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt: {e}")))?;
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    db.execute(
        "UPDATE notification_channels SET enabled = ?1, config_enc = ?2, updated_at = datetime('now') WHERE channel = ?3",
        rusqlite::params![enabled as i64, config_enc, channel],
    )?;
    Ok(())
}

fn test_message(state: &SharedState, channel_label: &str) -> AlertMessage {
    AlertMessage {
        rule_name: format!("[TEST] Pier notifications ({channel_label})"),
        severity: "info".to_string(),
        state: "firing".to_string(),
        metric: "deploy_status".to_string(),
        scope_label: "global".to_string(),
        server_label: crate::alerts::metrics::resolve_server_label(state, "global", None),
        value: None,
        threshold: None,
        comparison: "eq".to_string(),
        context: Some(format!(
            "If you see this, {channel_label} notifications work."
        )),
        time_str: crate::timezone::format_now(state),
    }
}

fn mask_url(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    if url.len() <= 16 {
        return "••••••••".to_string();
    }
    let head = &url[..url.find("://").map(|i| i + 3).unwrap_or(0).min(url.len())];
    format!("{head}••••••••{}", &url[url.len().saturating_sub(8)..])
}

fn read_channel(state: &SharedState, channel: &str) -> AppResult<(bool, String)> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let row = db
        .query_row(
            "SELECT enabled, config_enc FROM notification_channels WHERE channel = ?1",
            [channel],
            |row| Ok((row.get::<_, i64>(0)? == 1, row.get::<_, String>(1)?)),
        )
        .unwrap_or((false, String::new()));
    Ok(row)
}
