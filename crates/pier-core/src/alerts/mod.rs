pub mod channels;
pub mod evaluator;
pub mod hooks;
pub mod metrics;
pub mod types;

use chrono::Utc;
use tokio::time::{interval, Duration};

use crate::state::SharedState;
use types::{AlertMessage, AlertRule, EventKind};

const DEFAULT_CHECK_INTERVAL_SECS: u64 = 30;

pub fn start_scheduler(state: SharedState) {
    tokio::spawn(async move {
        // Small initial delay so DB migrations and other boot tasks settle first.
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut tick = interval(Duration::from_secs(
            read_check_interval(&state).unwrap_or(DEFAULT_CHECK_INTERVAL_SECS),
        ));
        loop {
            tick.tick().await;
            if let Err(e) = run_checks(&state).await {
                tracing::error!("Alerts scheduler error: {e}");
            }
        }
    });
}

fn read_check_interval(state: &SharedState) -> Option<u64> {
    let db = state.db.lock().ok()?;
    let v: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'alerts.check_interval_secs'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    v.parse().ok()
}

fn alerts_enabled(state: &SharedState) -> bool {
    let db = match state.db.lock() {
        Ok(d) => d,
        Err(_) => return true,
    };
    let v: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'alerts.enabled'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "true".to_string());
    v != "false"
}

async fn run_checks(state: &SharedState) -> anyhow::Result<()> {
    if !alerts_enabled(state) {
        return Ok(());
    }

    let rules = load_enabled_numeric_rules(state)?;
    for rule in rules {
        if let Err(e) = process_rule(state, rule).await {
            tracing::debug!("Alert rule error: {e}");
        }
    }
    Ok(())
}

fn load_enabled_numeric_rules(state: &SharedState) -> anyhow::Result<Vec<AlertRule>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                severity, channel, channel_config_enc, cooldown_mins, last_triggered_at,
                last_value, last_state, first_breach_at
         FROM alert_rules
         WHERE enabled = 1 AND threshold IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AlertRule {
            id: row.get(0)?,
            name: row.get(1)?,
            enabled: row.get::<_, i64>(2)? == 1,
            metric: row.get(3)?,
            scope: row.get(4)?,
            scope_id: row.get(5)?,
            threshold: row.get(6)?,
            comparison: row.get(7)?,
            duration_secs: row.get(8)?,
            severity: row.get(9)?,
            channel: row.get(10)?,
            channel_config_enc: row.get(11)?,
            cooldown_mins: row.get(12)?,
            last_triggered_at: row.get(13)?,
            last_value: row.get(14)?,
            last_state: row.get(15)?,
            first_breach_at: row.get(16)?,
        })
    })?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

async fn process_rule(state: &SharedState, rule: AlertRule) -> anyhow::Result<()> {
    // Skip event-based metrics here — they fire via hooks.
    if matches!(
        rule.metric.as_str(),
        "container_status" | "container_restarts" | "deploy_status" | "backup_status"
    ) {
        return Ok(());
    }

    let value =
        match metrics::fetch(state, &rule.metric, &rule.scope, rule.scope_id.as_deref()).await {
            Some(v) => v,
            None => return Ok(()),
        };

    let breached = evaluator::compare(value, rule.threshold.unwrap_or(0.0), &rule.comparison);

    // Update first_breach_at (enter/exit breach) and last_value
    update_breach_window(state, &rule, breached, value)?;

    // Re-load the rule (first_breach_at may have just changed)
    let rule = reload_rule(state, &rule.id)?.unwrap_or(rule);

    match evaluator::evaluate(&rule, value, Utc::now()) {
        EventKind::Fire => fire(state, &rule, Some(value)).await?,
        EventKind::Resolve => resolve(state, &rule, Some(value)).await?,
        EventKind::NoChange => {}
    }
    Ok(())
}

fn reload_rule(state: &SharedState, id: &str) -> anyhow::Result<Option<AlertRule>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let r = db
        .query_row(
            "SELECT id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                    severity, channel, channel_config_enc, cooldown_mins, last_triggered_at,
                    last_value, last_state, first_breach_at
             FROM alert_rules WHERE id = ?1",
            [id],
            |row| {
                Ok(AlertRule {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? == 1,
                    metric: row.get(3)?,
                    scope: row.get(4)?,
                    scope_id: row.get(5)?,
                    threshold: row.get(6)?,
                    comparison: row.get(7)?,
                    duration_secs: row.get(8)?,
                    severity: row.get(9)?,
                    channel: row.get(10)?,
                    channel_config_enc: row.get(11)?,
                    cooldown_mins: row.get(12)?,
                    last_triggered_at: row.get(13)?,
                    last_value: row.get(14)?,
                    last_state: row.get(15)?,
                    first_breach_at: row.get(16)?,
                })
            },
        )
        .ok();
    Ok(r)
}

fn update_breach_window(
    state: &SharedState,
    rule: &AlertRule,
    breached: bool,
    value: f64,
) -> anyhow::Result<()> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    if breached {
        if rule.first_breach_at.is_none() {
            db.execute(
                "UPDATE alert_rules SET first_breach_at = datetime('now'), last_value = ?1 WHERE id = ?2",
                rusqlite::params![value, rule.id],
            )?;
        } else {
            db.execute(
                "UPDATE alert_rules SET last_value = ?1 WHERE id = ?2",
                rusqlite::params![value, rule.id],
            )?;
        }
    } else if rule.first_breach_at.is_some() || rule.last_state == "firing" {
        db.execute(
            "UPDATE alert_rules SET first_breach_at = NULL, last_value = ?1 WHERE id = ?2",
            rusqlite::params![value, rule.id],
        )?;
    } else {
        db.execute(
            "UPDATE alert_rules SET last_value = ?1 WHERE id = ?2",
            rusqlite::params![value, rule.id],
        )?;
    }
    Ok(())
}

pub async fn fire(state: &SharedState, rule: &AlertRule, value: Option<f64>) -> anyhow::Result<()> {
    let msg = build_message(state, rule, "firing", value, None);
    send_and_record(state, rule, &msg, "firing", value).await
}

pub async fn resolve(
    state: &SharedState,
    rule: &AlertRule,
    value: Option<f64>,
) -> anyhow::Result<()> {
    let msg = build_message(state, rule, "resolved", value, None);
    send_and_record(state, rule, &msg, "resolved", value).await
}

pub async fn fire_with_context(
    state: &SharedState,
    rule: &AlertRule,
    value: Option<f64>,
    context: String,
) -> anyhow::Result<()> {
    let msg = build_message(state, rule, "firing", value, Some(context));
    send_and_record(state, rule, &msg, "firing", value).await
}

/// Resolve which channel credentials to use.
///
/// Rules from presets (migration 23) have an empty `channel_config_enc` and
/// rely on the global `notification_channels` table. Legacy rules keep their
/// own per-rule config. Returns `None` if the channel is disabled or has no
/// configuration set.
fn resolve_channel_config(state: &SharedState, rule: &AlertRule) -> anyhow::Result<Option<String>> {
    let key = crate::crypto::get_secret_key();

    if !rule.channel_config_enc.is_empty() {
        let plain = crate::crypto::decrypt(&rule.channel_config_enc, &key)
            .map_err(|e| anyhow::anyhow!("Decrypt per-rule channel_config: {e}"))?;
        return Ok(Some(plain));
    }

    // Fall back to the global channel entry.
    let (enabled, config_enc): (i64, String) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT enabled, config_enc FROM notification_channels WHERE channel = ?1",
            [&rule.channel],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap_or((0, String::new()))
    };

    if enabled != 1 || config_enc.is_empty() {
        return Ok(None);
    }

    let plain = crate::crypto::decrypt(&config_enc, &key)
        .map_err(|e| anyhow::anyhow!("Decrypt global channel_config: {e}"))?;
    Ok(Some(plain))
}

fn build_message(
    state: &SharedState,
    rule: &AlertRule,
    state_label: &str,
    value: Option<f64>,
    context: Option<String>,
) -> AlertMessage {
    AlertMessage {
        rule_name: rule.name.clone(),
        severity: rule.severity.clone(),
        state: state_label.to_string(),
        metric: rule.metric.clone(),
        scope_label: metrics::scope_label(state, &rule.scope, rule.scope_id.as_deref()),
        server_label: metrics::resolve_server_label(state, &rule.scope, rule.scope_id.as_deref()),
        value,
        threshold: rule.threshold,
        comparison: rule.comparison.clone(),
        context,
        time_str: crate::timezone::format_now(state),
    }
}

async fn send_and_record(
    state: &SharedState,
    rule: &AlertRule,
    msg: &AlertMessage,
    state_label: &str,
    value: Option<f64>,
) -> anyhow::Result<()> {
    let config_json = match resolve_channel_config(state, rule)? {
        Some(cfg) => cfg,
        None => {
            // Channel not configured or disabled — log and skip. Not an error:
            // the user simply hasn't set up notifications yet.
            tracing::debug!(
                "Alert '{}' would fire but channel '{}' is not configured — skipping",
                rule.name,
                rule.channel
            );
            return Ok(());
        }
    };

    let delivery = channels::send(&rule.channel, &config_json, msg).await;

    let event_id = uuid::Uuid::new_v4().to_string();
    let short_msg = format!("{} {} — {}", msg.severity, state_label, msg.scope_label);

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let (delivered, err): (i64, Option<String>) = match &delivery {
            Ok(_) => (1, None),
            Err(e) => (0, Some(e.to_string())),
        };
        db.execute(
            "INSERT INTO alert_events (id, rule_id, state, value, message, delivered, delivery_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![event_id, rule.id, state_label, value, short_msg, delivered, err],
        )?;

        if state_label == "firing" {
            db.execute(
                "UPDATE alert_rules SET last_triggered_at = datetime('now'), last_state = 'firing', updated_at = datetime('now') WHERE id = ?1",
                [&rule.id],
            )?;
        } else {
            db.execute(
                "UPDATE alert_rules SET last_state = 'ok', first_breach_at = NULL, updated_at = datetime('now') WHERE id = ?1",
                [&rule.id],
            )?;
        }
    }

    delivery
}
