//! Cron-expression helpers — expansion of shortcuts, parsing, preview.
//!
//! Pier accepts the conventional 5-field cron format (`m h dom mon dow`,
//! optionally a 6th `year` field) as well as the common shortcuts
//! (`@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`), with standard
//! cron semantics: day-of-week 0-7 where 0 and 7 are Sunday, names
//! `SUN`-`SAT` / `JAN`-`DEC`.
//!
//! The underlying `cron` crate wants a 7-field expression (seconds first,
//! year last) and numbers days of the week 1-7 with **1 = Sunday**. So we
//! validate every field ourselves, expand it to the set of values it means
//! and hand the crate an explicit list in its own numbering. The crate
//! never sees syntax it could read differently, and users get errors that
//! name the field they typed instead of the crate's shifted field names.
//!
//! A 7-field expression is taken as crate-native and passed through as is.

use std::fmt;
use std::str::FromStr;

use anyhow::Result;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// One field of a standard 5-field cron expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronField {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl CronField {
    const ORDER: [CronField; 5] = [
        CronField::Minute,
        CronField::Hour,
        CronField::DayOfMonth,
        CronField::Month,
        CronField::DayOfWeek,
    ];

    /// Inclusive range of values the field accepts (day-of-week 7 = Sunday).
    fn range(self) -> (u32, u32) {
        match self {
            CronField::Minute => (0, 59),
            CronField::Hour => (0, 23),
            CronField::DayOfMonth => (1, 31),
            CronField::Month => (1, 12),
            CronField::DayOfWeek => (0, 7),
        }
    }

    fn i18n_key(self) -> &'static str {
        match self {
            CronField::Minute => "errors.cron.field_minute",
            CronField::Hour => "errors.cron.field_hour",
            CronField::DayOfMonth => "errors.cron.field_day_of_month",
            CronField::Month => "errors.cron.field_month",
            CronField::DayOfWeek => "errors.cron.field_day_of_week",
        }
    }

    /// A single value: a number in range, or a month / weekday name.
    fn value(self, s: &str) -> Option<u32> {
        let (min, max) = self.range();
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            return s.parse().ok().filter(|v| (min..=max).contains(v));
        }
        const MONTHS: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let lower = s.to_ascii_lowercase();
        let pos = |names: &[&str]| names.iter().position(|n| *n == lower).map(|i| i as u32);
        match self {
            CronField::Month => pos(&MONTHS).map(|i| i + 1),
            CronField::DayOfWeek => pos(&DAYS),
            _ => None,
        }
    }
}

/// Why a cron expression was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    /// Not 5 fields (or 6 with a year, or 7 crate-native).
    FieldCount(usize),
    /// A field holds a value, range or step that is not valid for it.
    Field { field: CronField, value: String },
    /// Fields look fine but the expression still can't be scheduled.
    Invalid,
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::FieldCount(n) => write!(
                f,
                "expected 5 cron fields (minute hour day-of-month month day-of-week), got {n}"
            ),
            CronError::Field { field, value } => {
                let (min, max) = field.range();
                write!(f, "invalid {field:?} value '{value}' (allowed {min}-{max})")
            }
            CronError::Invalid => write!(f, "invalid cron expression"),
        }
    }
}

impl std::error::Error for CronError {}

/// Expand one field to the bit set of values it selects.
///
/// Items are comma-separated; each is `*`, `?` (day fields only), `N`,
/// `A-B`, `*/S`, `A/S` (A to the field maximum) or `A-B/S`.
fn expand(field: CronField, text: &str) -> std::result::Result<u64, CronError> {
    let err = || CronError::Field {
        field,
        value: text.to_string(),
    };
    let (min, max) = field.range();
    let mut mask = 0u64;
    for item in text.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, s)) => {
                let s: u32 = s
                    .bytes()
                    .all(|c| c.is_ascii_digit())
                    .then(|| s.parse::<u32>().ok())
                    .flatten()
                    .filter(|s| (1..=max.max(1)).contains(s))
                    .ok_or_else(err)?;
                (b, Some(s))
            }
            None => (item, None),
        };
        let any = base == "*"
            || (base == "?" && matches!(field, CronField::DayOfMonth | CronField::DayOfWeek));
        let (lo, hi) = if any {
            (min, max)
        } else if let Some((a, b)) = base.split_once('-') {
            let a = field.value(a).ok_or_else(err)?;
            let b = field.value(b).ok_or_else(err)?;
            if a > b {
                return Err(err());
            }
            (a, b)
        } else {
            let a = field.value(base).ok_or_else(err)?;
            (a, if step.is_some() { max } else { a })
        };
        let step = step.unwrap_or(1);
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok(mask)
}

/// Render one validated field in the `cron` crate's terms: `*` when every
/// value is selected, otherwise an explicit list. Day-of-week is moved to
/// the crate's numbering (standard 0/7 = Sunday → crate 1 = Sunday).
fn translate(field: CronField, text: &str) -> std::result::Result<String, CronError> {
    let mask = expand(field, text)?;
    let values: Vec<u32> = if field == CronField::DayOfWeek {
        // Fold 7 onto 0 (both Sunday), then shift to 1..=7.
        let days = (mask & 0x7f) | ((mask >> 7) & 1);
        (0..7)
            .filter(|d| days & (1 << d) != 0)
            .map(|d| d + 1)
            .collect()
    } else {
        let (min, max) = field.range();
        (min..=max).filter(|v| mask & (1 << v) != 0).collect()
    };
    let (min, max) = field.range();
    let full = if field == CronField::DayOfWeek {
        7
    } else {
        (max - min + 1) as usize
    };
    if values.len() == full {
        return Ok("*".to_string());
    }
    Ok(values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(","))
}

/// Turn a 5/6-field or shortcut expression into the 7-field form the
/// `cron` crate wants, validating every field on the way.
pub fn normalise(expr: &str) -> std::result::Result<String, CronError> {
    let trimmed = expr.trim();
    let source = match trimmed {
        "@yearly" | "@annually" => "0 0 1 1 *",
        "@monthly" => "0 0 1 * *",
        "@weekly" => "0 0 * * 0",
        "@daily" | "@midnight" => "0 0 * * *",
        "@hourly" => "0 * * * *",
        other => other,
    };
    let fields: Vec<&str> = source.split_whitespace().collect();
    match fields.len() {
        5 | 6 => {
            let mut out = vec!["0".to_string()];
            for (field, text) in CronField::ORDER.iter().zip(&fields) {
                out.push(translate(*field, text)?);
            }
            // Optional year: the crate validates it.
            out.push(
                fields
                    .get(5)
                    .map_or_else(|| "*".to_string(), |y| y.to_string()),
            );
            Ok(out.join(" "))
        }
        7 => Ok(fields.join(" ")),
        n => Err(CronError::FieldCount(n)),
    }
}

fn schedule(expr: &str) -> std::result::Result<cron::Schedule, CronError> {
    let normalised = normalise(expr)?;
    cron::Schedule::from_str(&normalised).map_err(|_| CronError::Invalid)
}

/// Parse a cron expression in any of the accepted forms. Returns the
/// crate-native [`cron::Schedule`].
pub fn parse(expr: &str) -> Result<cron::Schedule> {
    Ok(schedule(expr)?)
}

/// Check an expression typed by a user. The error is a localized,
/// human-readable message naming the offending field.
pub fn validate(expr: &str) -> std::result::Result<(), String> {
    use crate::i18n::{te, te_args};
    match schedule(expr) {
        Ok(_) => Ok(()),
        Err(CronError::FieldCount(n)) => Err(te_args(
            "errors.cron.field_count",
            &[("count", &n.to_string())],
        )),
        Err(CronError::Field { field, value }) => {
            let (min, max) = field.range();
            Err(te_args(
                "errors.cron.bad_value",
                &[
                    ("field", &te(field.i18n_key())),
                    ("value", &value),
                    ("range", &format!("{min}–{max}")),
                ],
            ))
        }
        Err(CronError::Invalid) => Err(te_args("errors.cron.invalid", &[("expr", expr.trim())])),
    }
}

/// Resolve a timezone label like "UTC" / "Europe/Moscow". Falls back to
/// UTC on unknown input so we never panic at runtime.
pub fn parse_tz(label: &str) -> Tz {
    Tz::from_str(label).unwrap_or(chrono_tz::UTC)
}

/// Compute the next `count` fire times after `after` for the given
/// expression, in the given timezone. Returns RFC3339 strings (UTC) for
/// transport to the UI.
pub fn preview(
    expr: &str,
    tz_label: &str,
    after: DateTime<Utc>,
    count: usize,
) -> Result<Vec<String>> {
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
pub fn next_fire_utc(
    expr: &str,
    tz_label: &str,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let schedule = parse(expr)?;
    let tz = parse_tz(tz_label);
    let after_tz = after.with_timezone(&tz);
    Ok(schedule
        .after(&after_tz)
        .next()
        .map(|t| t.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Weekday};

    fn weekdays(expr: &str, n: usize) -> Vec<Weekday> {
        parse(expr)
            .unwrap()
            .upcoming(Utc)
            .take(n)
            .map(|t| t.weekday())
            .collect()
    }

    #[test]
    fn shortcuts_expand() {
        assert_eq!(normalise("@hourly").unwrap(), "0 0 * * * * *");
        assert_eq!(normalise("@daily").unwrap(), "0 0 0 * * * *");
        assert_eq!(normalise("@weekly").unwrap(), "0 0 0 * * 1 *");
    }

    #[test]
    fn five_field_pads_to_seven() {
        assert_eq!(normalise("0 2 * * *").unwrap(), "0 0 2 * * * *");
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

    #[test]
    fn sunday_is_zero_seven_and_name() {
        for expr in [
            "0 2 * * 0",
            "0 2 * * 7",
            "0 2 * * SUN",
            "0 2 * * sun",
            "@weekly",
        ] {
            assert!(
                weekdays(expr, 3).iter().all(|d| *d == Weekday::Sun),
                "{expr} must fire on Sundays"
            );
        }
    }

    #[test]
    fn weekday_ranges_use_standard_numbering() {
        for expr in ["0 9 * * 1-5", "0 9 * * MON-FRI"] {
            let days = weekdays(expr, 10);
            assert!(
                days.iter()
                    .all(|d| !matches!(d, Weekday::Sat | Weekday::Sun)),
                "{expr} fired on {days:?}"
            );
        }
        let days = weekdays("0 0 * * 5-7", 9);
        assert!(days
            .iter()
            .all(|d| matches!(d, Weekday::Fri | Weekday::Sat | Weekday::Sun)));
        let days = weekdays("0 0 * * */2", 8);
        assert!(days
            .iter()
            .all(|d| matches!(d, Weekday::Sun | Weekday::Tue | Weekday::Thu | Weekday::Sat)));
        assert_eq!(normalise("0 0 * * 1-5").unwrap(), "0 0 0 * * 2,3,4,5,6 *");
        assert_eq!(normalise("0 0 * * 0-7").unwrap(), "0 0 0 * * * *");
    }

    #[test]
    fn other_fields_expand_to_lists() {
        assert_eq!(normalise("*/15 * * * *").unwrap(), "0 0,15,30,45 * * * * *");
        assert_eq!(
            normalise("0 9-11 * JAN,jun *").unwrap(),
            "0 0 9,10,11 * 1,6 * *"
        );
        assert_eq!(normalise("30 3 ? * ?").unwrap(), "0 30 3 * * * *");
    }

    #[test]
    fn errors_name_the_field() {
        let field = |expr: &str| match normalise(expr) {
            Err(CronError::Field { field, value }) => (field, value),
            other => panic!("{expr}: {other:?}"),
        };
        assert_eq!(field("0 25 * * *"), (CronField::Hour, "25".into()));
        assert_eq!(field("61 * * * *"), (CronField::Minute, "61".into()));
        assert_eq!(field("0 2 * 13 *").0, CronField::Month);
        assert_eq!(field("0 2 * * 8").0, CronField::DayOfWeek);
        assert_eq!(field("0 2 0 * *").0, CronField::DayOfMonth);
        assert_eq!(field("*/0 * * * *").0, CronField::Minute);
        assert_eq!(field("0 5-2 * * *").0, CronField::Hour);
        assert_eq!(field("0 2 * * every").0, CronField::DayOfWeek);
        assert_eq!(normalise("every day"), Err(CronError::FieldCount(2)));
        assert_eq!(normalise(""), Err(CronError::FieldCount(0)));
    }
}
