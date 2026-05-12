//! TOTP (RFC 6238) wrappers around `totp-rs` plus recovery-code helpers.
//!
//! Why a thin wrapper instead of using `totp-rs` directly from handlers?
//!   - All Pier-side defaults (issuer, digit count, ±1-step skew window) live
//!     in one place. Tweaking algorithm or window doesn't ripple across files.
//!   - Recovery codes need a separate code path (random generate, SHA-256 at
//!     rest, one-shot consume). Lumping them with the TOTP primitives keeps
//!     auth/handlers.rs declarative.

use anyhow::{anyhow, Result};
use rand::RngExt;
use sha2::{Digest, Sha256};
use totp_rs::{Algorithm, Secret, TOTP};

/// Issuer string shown by authenticator apps (Google Authenticator, Aegis…).
/// Kept generic — every Pier instance carries the same label.
pub const TOTP_ISSUER: &str = "Pier";

/// Generate a fresh, base32-encoded 20-byte secret. RFC 4226 §4 recommends
/// at least 128 bits of entropy; 160 matches `totp-rs::Secret::generate_secret()`.
pub fn generate_secret() -> String {
    let bytes: [u8; 20] = rand::rng().random();
    Secret::Raw(bytes.to_vec()).to_encoded().to_string()
}

/// Build a `TOTP` configured to Pier defaults.
fn totp_for(account: &str, b32_secret: &str) -> Result<TOTP> {
    let raw = Secret::Encoded(b32_secret.to_string())
        .to_bytes()
        .map_err(|e| anyhow!("invalid TOTP secret: {e:?}"))?;
    TOTP::new(
        Algorithm::SHA1,
        6,  // digits — what every consumer-grade app expects
        1,  // skew steps — accept the previous + next code (handles clock drift)
        30, // time step in seconds
        raw,
        Some(TOTP_ISSUER.into()),
        account.to_string(),
    )
    .map_err(|e| anyhow!("build TOTP: {e:?}"))
}

/// Render the `otpauth://totp/Pier:<account>?secret=…&issuer=Pier` URI that
/// authenticator apps consume. Suitable for QR encoding or manual entry.
pub fn otpauth_url(account: &str, b32_secret: &str) -> Result<String> {
    Ok(totp_for(account, b32_secret)?.get_url())
}

/// Validate a 6-digit code against the secret. Honours the ±1 step window
/// configured in `totp_for`.
pub fn check(b32_secret: &str, account: &str, code: &str) -> Result<bool> {
    let totp = totp_for(account, b32_secret)?;
    Ok(totp.check_current(code).unwrap_or(false))
}

// ── Recovery codes ──────────────────────────────────────────────

/// Generate `count` user-facing recovery codes (format `XXXX-XXXX-XXXX`,
/// crockford-style alphabet excluding ambiguous chars).
pub fn generate_recovery_codes(count: usize) -> Vec<String> {
    const ALPHA: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no I/O/0/1
    let mut out = Vec::with_capacity(count);
    let mut rng = rand::rng();
    for _ in 0..count {
        let mut chars = [0u8; 12];
        for slot in chars.iter_mut() {
            // 32-character alphabet → mask of 5 bits, no modulo bias.
            let r: u8 = rng.random();
            *slot = ALPHA[(r & 0x1f) as usize];
        }
        let s = std::str::from_utf8(&chars).expect("ascii alphabet");
        out.push(format!("{}-{}-{}", &s[0..4], &s[4..8], &s[8..12]));
    }
    out
}

/// SHA-256 hex of the code, with normalisation. We accept user input with
/// dashes and mixed case (`abcd-EFGH-1234` matches the stored hash of `ABCDEFGH1234`).
pub fn hash_recovery_code(code: &str) -> String {
    let normalised: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let mut h = Sha256::new();
    h.update(normalised.as_bytes());
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Returns Some(remaining JSON) on success — the matched hash is removed so
/// the caller can persist the updated list back to the DB. Returns None when
/// no entry matches.
pub fn consume_recovery_code(stored_json: &str, provided: &str) -> Option<String> {
    let hashes: Vec<String> = serde_json::from_str(stored_json).ok()?;
    let target = hash_recovery_code(provided);
    if !hashes.iter().any(|h| h == &target) {
        return None;
    }
    let remaining: Vec<&String> = hashes.iter().filter(|h| **h != target).collect();
    serde_json::to_string(&remaining).ok()
}

/// SVG QR rendering of any string (typically an otpauth:// URL). 200×200,
/// `quiet_zone = true` so the result is scannable without extra margin in CSS.
pub fn qr_svg(content: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let code = QrCode::new(content.as_bytes()).map_err(|e| anyhow!("qr encode: {e}"))?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .dark_color(svg::Color("#111827"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_code_hash_is_case_and_dash_insensitive() {
        let h1 = hash_recovery_code("ABCD-EFGH-1234");
        let h2 = hash_recovery_code("abcdefgh1234");
        let h3 = hash_recovery_code("abcd efgh 1234");
        assert_eq!(h1, h2);
        assert_eq!(h1, h3);
    }

    #[test]
    fn generated_recovery_codes_have_expected_shape() {
        let codes = generate_recovery_codes(10);
        assert_eq!(codes.len(), 10);
        for c in &codes {
            assert_eq!(c.len(), 14);
            assert_eq!(c.as_bytes()[4], b'-');
            assert_eq!(c.as_bytes()[9], b'-');
        }
    }

    #[test]
    fn consume_recovery_code_removes_only_match() {
        let hashes = vec![
            hash_recovery_code("AAAA-BBBB-CCCC"),
            hash_recovery_code("DDDD-EEEE-FFFF"),
        ];
        let json = serde_json::to_string(&hashes).unwrap();
        // Wrong code → None
        assert!(consume_recovery_code(&json, "0000-0000-0000").is_none());
        // Right code → JSON with the other one remaining
        let after = consume_recovery_code(&json, "aaaabbbbcccc").unwrap();
        let remaining: Vec<String> = serde_json::from_str(&after).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0], hash_recovery_code("DDDD-EEEE-FFFF"));
    }
}
