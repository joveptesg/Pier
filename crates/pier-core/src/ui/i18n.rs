//! Translation lookup + locale negotiation.
//!
//! Translations live in `assets/locales/{locale}.toml` and are embedded at
//! build time via RustEmbed. Each file is a flat dotted-key TOML — nested
//! `[section]` tables are flattened to `section.key` strings when loaded.
//!
//! Lookup falls back from the requested locale → English → the raw key, so
//! missing entries degrade gracefully without panicking or showing blanks.

// SUPPORTED / negotiate / detect_locale / table_for are wired into page
// handlers in the follow-up commit; suppress until they have call sites.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/locales/"]
struct LocaleAssets;

/// Locales we ship translation tables for. Anything outside this set
/// falls back to `DEFAULT` even if a `<lang>.toml` shows up on disk.
pub const SUPPORTED: &[&str] = &["en", "ru", "zh-CN", "de", "es", "fr", "ja", "pt-BR"];

/// Locale used when none of the user's preferences match.
pub const DEFAULT: &str = "en";

static TABLES: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();

fn tables() -> &'static HashMap<String, HashMap<String, String>> {
    TABLES.get_or_init(|| {
        let mut all = HashMap::new();
        for path in LocaleAssets::iter() {
            let Some(file) = LocaleAssets::get(&path) else {
                continue;
            };
            let Ok(content) = std::str::from_utf8(file.data.as_ref()) else {
                continue;
            };
            let locale = path.trim_end_matches(".toml").to_string();
            let parsed: toml::Value = match toml::from_str(content) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Failed to parse locale {locale}: {e}");
                    continue;
                }
            };
            let mut flat = HashMap::new();
            flatten(&parsed, "", &mut flat);
            tracing::info!("Loaded locale {locale} ({} keys)", flat.len());
            all.insert(locale, flat);
        }
        all
    })
}

fn flatten(value: &toml::Value, prefix: &str, out: &mut HashMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(v, &key, out);
            }
        }
        toml::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        _ => {}
    }
}

/// Translate a key for the requested locale. Falls back to English, then to
/// the raw key — never panics, always returns something renderable.
pub fn t(locale: &str, key: &str) -> String {
    let tables = tables();
    if let Some(table) = tables.get(locale) {
        if let Some(v) = table.get(key) {
            return v.clone();
        }
    }
    if locale != DEFAULT {
        if let Some(en) = tables.get(DEFAULT) {
            if let Some(v) = en.get(key) {
                return v.clone();
            }
        }
    }
    key.to_string()
}

/// Pick the best supported locale from an Accept-Language–style header value.
/// Handles language-only fallback (`en-US` → `en`).
pub fn negotiate(preferred: Option<&str>) -> String {
    let Some(value) = preferred else {
        return DEFAULT.to_string();
    };
    for tag in value.split(',') {
        let tag = tag.split(';').next().unwrap_or("").trim();
        if SUPPORTED.contains(&tag) {
            return tag.to_string();
        }
        let lang = tag.split('-').next().unwrap_or("");
        if SUPPORTED.contains(&lang) {
            return lang.to_string();
        }
    }
    DEFAULT.to_string()
}

/// Detect locale from request headers: cookie `pier_locale` overrides
/// the `Accept-Language` header; both fall back to `DEFAULT`.
pub fn detect_locale(headers: &axum::http::HeaderMap) -> String {
    if let Some(cookie) = headers.get(axum::http::header::COOKIE) {
        if let Ok(s) = cookie.to_str() {
            for kv in s.split(';') {
                if let Some((k, v)) = kv.trim().split_once('=') {
                    if k == "pier_locale" && SUPPORTED.contains(&v) {
                        return v.to_string();
                    }
                }
            }
        }
    }
    if let Some(al) = headers.get(axum::http::header::ACCEPT_LANGUAGE) {
        if let Ok(s) = al.to_str() {
            return negotiate(Some(s));
        }
    }
    DEFAULT.to_string()
}

/// Return the full translation table for a locale, for client-side use
/// (Alpine `t(key)` helper reads from a JSON blob injected per page).
pub fn table_for(locale: &str) -> HashMap<String, String> {
    let tables = tables();
    let mut out = tables.get(DEFAULT).cloned().unwrap_or_default();
    if locale != DEFAULT {
        if let Some(localized) = tables.get(locale) {
            for (k, v) in localized {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_handles_nested_tables() {
        let v: toml::Value = toml::from_str(
            r#"
            [settings.tab]
            cleanup = "Cleanup"
            "#,
        )
        .unwrap();
        let mut out = HashMap::new();
        flatten(&v, "", &mut out);
        assert_eq!(out.get("settings.tab.cleanup").map(String::as_str), Some("Cleanup"));
    }

    #[test]
    fn t_falls_back_to_english_then_to_key() {
        assert_eq!(t("en", "settings.tab.cleanup"), "Cleanup");
        assert_eq!(t("zz", "settings.tab.cleanup"), "Cleanup"); // unknown locale → en
        assert_eq!(t("en", "nonexistent.key"), "nonexistent.key"); // missing → raw key
    }

    #[test]
    fn negotiate_picks_supported_with_quality_and_region_fallback() {
        assert_eq!(negotiate(Some("ru,en;q=0.5")), "ru");
        assert_eq!(negotiate(Some("en-US,en;q=0.9")), "en");
        assert_eq!(negotiate(Some("xx-YY")), "en");
        assert_eq!(negotiate(None), "en");
    }
}
