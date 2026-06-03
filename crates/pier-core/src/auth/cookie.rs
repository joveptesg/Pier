//! Session cookie helpers shared between auth handlers and the auth middleware.
//!
//! Centralising these strings ensures that the cookie attributes we *set* and
//! the cookie attributes we use to *clear* an entry are always in sync — a
//! mismatched `Path`, `Secure`, or `SameSite` between the two would leave a
//! stale entry on the client.

use crate::config::TlsMode;
use crate::state::SharedState;

/// Build a `Set-Cookie` header for the session.
///
/// `Secure` is set whenever TLS termination is in-process. We deliberately do
/// not set it when `tls_mode == Off` so that an operator who terminates TLS at
/// a separate reverse proxy and runs Pier on plain HTTP locally still gets a
/// working session cookie.
///
/// `SameSite=Lax` (not `Strict`) because the panel takes part in OAuth-style
/// return flows where a third-party site redirects the operator back to a
/// Pier URL via top-level navigation — e.g. GitHub redirecting from the
/// "install App" page to `/sources?installation_id=…`. `Strict` would strip
/// the cookie on those redirects and dump the operator to `/login`. `Lax`
/// still blocks the dangerous CSRF cases (cross-site POST/fetch/iframe),
/// which is what we actually care about.
pub fn build_session_cookie(state: &SharedState, value: &str, max_age_secs: i64) -> String {
    build_session_cookie_str(
        &state.config.session_cookie,
        value,
        max_age_secs,
        state.config.tls_mode == TlsMode::Off,
    )
}

/// Build the headers needed to delete the session cookie on the client.
///
/// Returns up to two `Set-Cookie` values. The second variant is emitted when
/// `tls_mode != Off`: it omits `Secure` so that any leftover cookie a previous
/// Pier version (or a previous operator config) set without `Secure` is also
/// cleared. Browsers treat `pier_session` set with and without `Secure` as
/// distinct entries — without this backstop the operator can end up with two
/// `pier_session` cookies on one domain, one of which is always stale.
pub fn clear_session_cookies(state: &SharedState) -> Vec<String> {
    clear_session_cookies_str(
        &state.config.session_cookie,
        state.config.tls_mode == TlsMode::Off,
    )
}

fn build_session_cookie_str(name: &str, value: &str, max_age_secs: i64, tls_off: bool) -> String {
    // A `__Host-`-prefixed cookie is rejected by the browser unless it is
    // `Secure` (and host-only with `Path=/`, which we always emit). When the
    // operator keeps the default `__Host-` name we therefore force `Secure`
    // even under `tls_mode == Off`: in a TLS-terminating-proxy setup the
    // browser-facing hop is still HTTPS, so the cookie is valid and sent back.
    // Only a plain (non-`__Host-`) name honours the `tls_off` opt-out.
    let force_secure = name.starts_with("__Host-");
    let secure = if tls_off && !force_secure {
        ""
    } else {
        "Secure; "
    };
    format!("{name}={value}; Path=/; HttpOnly; {secure}SameSite=Lax; Max-Age={max_age_secs}")
}

fn clear_session_cookies_str(name: &str, tls_off: bool) -> Vec<String> {
    let primary = build_session_cookie_str(name, "", 0, tls_off);
    // A `__Host-` cookie is always Secure + host-only, so a single matching
    // clear is exact — no insecure sibling can exist to need a backstop.
    if tls_off || name.starts_with("__Host-") {
        vec![primary]
    } else {
        let without_secure = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
        vec![primary, without_secure]
    }
}

#[cfg(test)]
mod tests {
    use super::{build_session_cookie_str, clear_session_cookies_str};

    #[test]
    fn cookie_with_tls_includes_secure() {
        let v = build_session_cookie_str("pier_session", "abc", 3600, false);
        assert!(v.contains("Secure"), "v = {v}");
        assert!(v.contains("HttpOnly"), "v = {v}");
        assert!(v.contains("SameSite=Lax"), "v = {v}");
        assert!(v.contains("Max-Age=3600"), "v = {v}");
        assert!(v.starts_with("pier_session=abc;"), "v = {v}");
    }

    #[test]
    fn cookie_without_tls_omits_secure() {
        let v = build_session_cookie_str("pier_session", "abc", 3600, true);
        assert!(!v.contains("Secure"), "v = {v}");
        assert!(v.contains("HttpOnly"), "v = {v}");
    }

    #[test]
    fn clear_with_tls_emits_both_variants() {
        let cookies = clear_session_cookies_str("pier_session", false);
        assert_eq!(cookies.len(), 2, "cookies = {cookies:?}");
        // Primary clear cookie matches the live-cookie attribute set so the
        // browser actually replaces the entry instead of inserting a sibling.
        assert!(cookies[0].contains("Secure"), "cookies = {cookies:?}");
        assert!(cookies[0].contains("Max-Age=0"), "cookies = {cookies:?}");
        // Backstop: clear any cookie a previous epoch set without Secure.
        assert!(!cookies[1].contains("Secure"), "cookies = {cookies:?}");
        assert!(cookies[1].contains("Max-Age=0"), "cookies = {cookies:?}");
    }

    #[test]
    fn clear_without_tls_emits_single_variant() {
        let cookies = clear_session_cookies_str("pier_session", true);
        assert_eq!(cookies.len(), 1, "cookies = {cookies:?}");
        assert!(!cookies[0].contains("Secure"), "cookies = {cookies:?}");
        assert!(cookies[0].contains("Max-Age=0"), "cookies = {cookies:?}");
    }

    #[test]
    fn cookie_name_is_honoured() {
        let v = build_session_cookie_str("custom_name", "v", 0, true);
        assert!(v.starts_with("custom_name=v;"), "v = {v}");
    }

    #[test]
    fn host_prefixed_cookie_forces_secure_even_when_tls_off() {
        // `tls_off == true` would normally drop `Secure`, but a `__Host-` name
        // must keep it or the browser rejects the cookie outright.
        let v = build_session_cookie_str("__Host-pier_session", "abc", 3600, true);
        assert!(v.contains("Secure"), "v = {v}");
        assert!(v.contains("Path=/"), "v = {v}");
        assert!(v.contains("HttpOnly"), "v = {v}");
        // `__Host-` also forbids a Domain attribute — we never emit one.
        assert!(!v.contains("Domain"), "v = {v}");
    }

    #[test]
    fn host_prefixed_clear_emits_single_secure_variant() {
        // Even with TLS terminated in-process (tls_off == false) the legacy
        // non-Secure backstop is skipped for `__Host-` names: an insecure
        // sibling can't exist, and emitting one would be a rejected cookie.
        let cookies = clear_session_cookies_str("__Host-pier_session", false);
        assert_eq!(cookies.len(), 1, "cookies = {cookies:?}");
        assert!(cookies[0].contains("Secure"), "cookies = {cookies:?}");
        assert!(cookies[0].contains("Max-Age=0"), "cookies = {cookies:?}");
    }
}
