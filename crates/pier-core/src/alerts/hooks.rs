use super::types::AlertRule;
use crate::state::SharedState;

/// Fire all matching event-based rules for a given metric + scope.
///
/// Used by deploy/backup/container code paths when a meaningful event occurs.
/// Matching: `metric` equals, and scope matches OR rule is global.
pub async fn fire_event(
    state: &SharedState,
    metric: &str,
    service_id: Option<&str>,
    context: String,
) {
    let rules = match load_event_rules(state, metric, service_id) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("hooks load rules: {e}");
            return;
        }
    };
    for rule in rules {
        if let Err(e) = super::fire_with_context(state, &rule, None, context.clone()).await {
            tracing::warn!("Alert event fire failed for rule {}: {e}", rule.id);
        }
    }
}

fn load_event_rules(
    state: &SharedState,
    metric: &str,
    service_id: Option<&str>,
) -> anyhow::Result<Vec<AlertRule>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, name, enabled, metric, scope, scope_id, threshold, comparison, duration_secs,
                severity, channel, channel_config_enc, cooldown_mins, last_triggered_at,
                last_value, last_state, first_breach_at
         FROM alert_rules
         WHERE enabled = 1 AND metric = ?1
               AND (scope = 'global' OR scope_id = ?2 OR scope_id IS NULL)",
    )?;
    let rows = stmt.query_map(rusqlite::params![metric, service_id], |row| {
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
