//! Cron-expression helpers — expansion of shortcuts, parsing, preview.
//!
//! Pier accepts the conventional 5-field cron format (`m h dom mon dow`)
//! as well as the common shortcuts (`@hourly`, `@daily`, `@weekly`,
//! `@monthly`, `@yearly`). The underlying `cron` crate, however, expects
//! a 7-field expression (with seconds and year). We pad the input on the
//! way in so the rest of the codebase only deals with the standard form.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::str::FromStr;

/// Expand a 5-field or shortcut cron expression into the 7-field form the
/// `cron` crate wants.
///
/// Returns the normalised string. Invalid input still produces a string
/// — the actual parse error surfaces from [`parse`].
pub fn normalise(expr: &str) -> String {
    let trimmed = expr.trim();
    match trimmed {
        "@yearly" | "@annually" => "0 0 0 1 1 * *".to_string(),
        "@monthly" => "0 0 0 1 * * *".to_string(),
        "@weekly" => "0 0 0 * * 0 *".to_string(),
        "@daily" | "@midnight" => "0 0 0 * * * *".to_string(),
        "@hourly" => "0 0 * * * * *".to_string(),
        _ => {
            let fields: Vec<&str> = trimmed.split_whitespace().collect();
            match fields.len() {
                5 => format!("0 {} *", fields.join(" ")),
                6 => format!("0 {}", fields.join(" ")),
                7 => fields.join(" "),
                _ => trimmed.to_string(), // let cron::parse fail with a real message
            }
        }
    }
}

/// Parse a cron expression in any of the accepted forms. Returns the
/// crate-native [`cron::Schedule`].
pub fn parse(expr: &str) -> Result<cron::Schedule> {
    let normalised = normalise(expr);
    cron::Schedule::from_str(&normalised)
        .map_err(|e| anyhow!("invalid cron expression '{expr}': {e}"))
}

/// Resolve a timezone label like "UTC" / "Europe/Moscow". Falls back to
/// UTC on unknown input so we never panic at runtime.
pub fn parse_tz(label: &str) -> Tz {
    Tz::from_str(label).unwrap_or(chrono_tz::UTC)
}

/// Compute the next `count` fire times after `after` for the given
/// expression, in the given timezone. Returns RFC3339 strings (UTC) for
/// transport to the UI.
pub fn preview(expr: &str, tz_label: &str, after: DateTime<Utc>, count: usize) -> Result<Vec<String>> {
    let schedule = parse(expr)?;
    let tz = parse_tz(tz_label);
    let after_tz = after.with_timezone(&tz);
    let times: Vec<String> = schedule
        .after(&after_tz)
        .take(count)
        .map(|t| t.with_timezone(&Utc).to_rfc3339())
        .collect();
    Ok(times)
}

/// Pick the next single fire time for storing in `schedules.next_run_at`.
pub fn next_fire_utc(expr: &str, tz_label: &str, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let schedule = parse(expr)?;
    let tz = parse_tz(tz_label);
    let after_tz = after.with_timezone(&tz);
    Ok(schedule.after(&after_tz).next().map(|t| t.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_expand() {
        assert_eq!(normalise("@hourly"), "0 0 * * * * *");
        assert_eq!(normalise("@daily"), "0 0 0 * * * *");
        assert_eq!(normalise("@weekly"), "0 0 0 * * 0 *");
    }

    #[test]
    fn five_field_pads_to_seven() {
        assert_eq!(normalise("0 2 * * *"), "0 0 2 * * * *");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("not a cron").is_err());
    }

    #[test]
    fn parse_accepts_standard() {
        assert!(parse("0 2 * * *").is_ok());
        assert!(parse("@hourly").is_ok());
    }

    #[test]
    fn preview_returns_five() {
        let now = Utc::now();
        let nexts = preview("@hourly", "UTC", now, 5).unwrap();
        assert_eq!(nexts.len(), 5);
    }
}
