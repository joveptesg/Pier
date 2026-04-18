use chrono::{DateTime, Utc};

use super::types::{AlertRule, EventKind};

pub fn compare(value: f64, threshold: f64, op: &str) -> bool {
    match op {
        "gt" => value > threshold,
        "lt" => value < threshold,
        "eq" => (value - threshold).abs() < f64::EPSILON,
        _ => false,
    }
}

/// Decide whether a rule should fire, resolve, or stay unchanged given current value & time.
///
/// `now` — current time (UTC).
/// `value` — current metric reading.
///
/// Rules:
/// - Fire when breach holds for `duration_secs` AND cooldown has expired.
/// - Resolve when state is `firing` and value is back under threshold.
pub fn evaluate(rule: &AlertRule, value: f64, now: DateTime<Utc>) -> EventKind {
    let threshold = match rule.threshold {
        Some(t) => t,
        None => return EventKind::NoChange,
    };
    let breached = compare(value, threshold, &rule.comparison);

    match rule.last_state.as_str() {
        "firing" => {
            if !breached {
                return EventKind::Resolve;
            }
            // Still firing — maybe re-fire if cooldown expired
            if let Some(last) = parse_ts(&rule.last_triggered_at) {
                let elapsed = (now - last).num_minutes();
                if elapsed >= rule.cooldown_mins {
                    return EventKind::Fire;
                }
            }
            EventKind::NoChange
        }
        _ => {
            if !breached {
                return EventKind::NoChange;
            }
            // Need breach to persist for duration_secs
            let first_breach = parse_ts(&rule.first_breach_at).unwrap_or(now);
            let held_secs = (now - first_breach).num_seconds();
            if held_secs >= rule.duration_secs {
                EventKind::Fire
            } else {
                EventKind::NoChange
            }
        }
    }
}

fn parse_ts(s: &Option<String>) -> Option<DateTime<Utc>> {
    let s = s.as_ref()?;
    // SQLite datetime('now') format: "2026-04-18 14:32:10"
    let padded = format!("{s}+00:00");
    chrono::DateTime::parse_from_str(&padded, "%Y-%m-%d %H:%M:%S%:z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_rule() -> AlertRule {
        AlertRule {
            id: "r1".into(),
            name: "cpu".into(),
            enabled: true,
            metric: "cpu".into(),
            scope: "global".into(),
            scope_id: None,
            threshold: Some(80.0),
            comparison: "gt".into(),
            duration_secs: 60,
            severity: "warning".into(),
            channel: "telegram".into(),
            channel_config_enc: String::new(),
            cooldown_mins: 30,
            last_triggered_at: None,
            last_value: None,
            last_state: "ok".into(),
            first_breach_at: None,
        }
    }

    #[test]
    fn below_threshold_no_change() {
        let r = base_rule();
        let now = Utc::now();
        assert_eq!(evaluate(&r, 50.0, now), EventKind::NoChange);
    }

    #[test]
    fn above_threshold_but_short_breach() {
        let mut r = base_rule();
        let now = Utc::now();
        r.first_breach_at = Some(now.format("%Y-%m-%d %H:%M:%S").to_string());
        assert_eq!(evaluate(&r, 90.0, now), EventKind::NoChange);
    }

    #[test]
    fn above_threshold_sustained_fires() {
        let mut r = base_rule();
        let earlier = Utc::now() - chrono::Duration::seconds(120);
        r.first_breach_at = Some(earlier.format("%Y-%m-%d %H:%M:%S").to_string());
        assert_eq!(evaluate(&r, 90.0, Utc::now()), EventKind::Fire);
    }

    #[test]
    fn firing_resolves_when_under() {
        let mut r = base_rule();
        r.last_state = "firing".into();
        r.last_triggered_at = Some(Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());
        assert_eq!(evaluate(&r, 50.0, Utc::now()), EventKind::Resolve);
    }

    #[test]
    fn firing_respects_cooldown() {
        let mut r = base_rule();
        r.last_state = "firing".into();
        r.last_triggered_at = Some(Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());
        // Still breached, cooldown not elapsed
        assert_eq!(evaluate(&r, 90.0, Utc::now()), EventKind::NoChange);
    }

    #[test]
    fn firing_refires_after_cooldown() {
        let mut r = base_rule();
        r.last_state = "firing".into();
        let earlier = Utc::now() - chrono::Duration::minutes(45);
        r.last_triggered_at = Some(earlier.format("%Y-%m-%d %H:%M:%S").to_string());
        assert_eq!(evaluate(&r, 90.0, Utc::now()), EventKind::Fire);
    }
}
