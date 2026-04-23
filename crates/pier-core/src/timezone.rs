use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::state::SharedState;

const SETTINGS_KEY: &str = "system.timezone";
const DEFAULT_TZ: &str = "UTC";

/// Read the configured system timezone. Falls back to UTC on any failure.
pub fn current_tz(state: &SharedState) -> Tz {
    let name = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [SETTINGS_KEY],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_TZ.to_string());
    name.parse::<Tz>().unwrap_or(chrono_tz::UTC)
}

/// Format a UTC timestamp in the configured system timezone.
/// Shape: `2026-04-19 18:42 MSK`.
pub fn format_local(state: &SharedState, ts: DateTime<Utc>) -> String {
    let tz = current_tz(state);
    ts.with_timezone(&tz)
        .format("%Y-%m-%d %H:%M %Z")
        .to_string()
}

/// Format "now" in the configured system timezone.
pub fn format_now(state: &SharedState) -> String {
    format_local(state, Utc::now())
}
