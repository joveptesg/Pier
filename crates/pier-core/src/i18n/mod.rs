//! Internationalization (i18n) for the admin panel.
//!
//! The translation catalog is embedded at compile time by the
//! `rust_i18n::i18n!("locales", fallback = "en")` invocation in the crate root
//! (`main.rs`). English is the source locale.
//!
//! This module is being built incrementally (see the i18n plan):
//!   - Step 1 (here): a spike that pins down rust-i18n's runtime API shape.
//!   - Step 2: per-request locale via `tokio::task_local!`, an Axum middleware
//!     that resolves the locale, and the MiniJinja `t()` bridge.

#[cfg(test)]
mod spike_tests {
    //! Step-1 spike: confirm the two rust-i18n behaviours the design depends on,
    //! before building the full pipeline. If either assertion fails to compile
    //! or run, we fall back to a hand-rolled YAML catalog (see plan, Phase 0).

    /// `t!` must accept a **runtime** `&str` key and a **runtime** locale
    /// expression — our MiniJinja bridge passes both at runtime, they are never
    /// string literals.
    #[test]
    fn accepts_runtime_key_and_locale() {
        let key = String::from("spike.plain");
        let loc = String::from("en");

        let got = rust_i18n::t!(key.as_str(), locale = loc.as_str());
        assert_eq!(got, "Spike works");
    }

    /// With no interpolation vars passed, `t!` must leave `%{...}` placeholders
    /// intact — the MiniJinja `t()` function substitutes them itself from
    /// template kwargs, so it needs the raw string.
    #[test]
    fn leaves_placeholders_untouched_when_no_vars() {
        let key = String::from("spike.interp");
        let loc = String::from("en");

        let raw = rust_i18n::t!(key.as_str(), locale = &loc);
        assert!(
            raw.contains("%{name}"),
            "expected raw placeholder to survive, got: {raw}"
        );
    }

    /// Unknown locales must fall back to the source locale (`en`) rather than
    /// returning the key or panicking.
    #[test]
    fn falls_back_to_source_locale() {
        let loc = String::from("zz-XX");
        let got = rust_i18n::t!("spike.plain", locale = loc.as_str());
        assert_eq!(got, "Spike works");
    }
}
