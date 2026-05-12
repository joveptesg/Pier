use anyhow::Result;
use zxcvbn::{zxcvbn, Score};

/// Minimum admin password length, enforced before zxcvbn so that short-but-
/// "creative" passwords still get rejected without burning cycles on entropy
/// estimation. 12 is the modern floor for human-typed admin credentials; the
/// generator UI produces 32-char passwords by default and easily clears this.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Reject weak admin passwords.
///
/// Combines a hard length floor with `zxcvbn` entropy scoring. The `user_inputs`
/// list (typically the chosen username and email) lets zxcvbn penalise passwords
/// derived from those fields — e.g. `admin@example.com` → `Admin1234!` would score
/// poorly. We require score ≥ 3 ("Can be cracked with 10^10 guesses or less"),
/// matching the zxcvbn project's own recommendation for security-sensitive
/// accounts.
///
/// The returned error message is intentionally generic (no rule-by-rule breakdown)
/// — we don't want to give an attacker probing the endpoint a cheap signal about
/// which dimension to strengthen.
pub fn validate_password_strength(password: &str, user_inputs: &[&str]) -> Result<(), String> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    let estimate = zxcvbn(password, user_inputs);
    if estimate.score() < Score::Three {
        return Err("Password is too weak".into());
    }
    Ok(())
}

/// Hash a plaintext password with bcrypt (cost=12).
pub fn hash_password(password: &str) -> Result<String> {
    bcrypt::hash(password, 12).map_err(|e| anyhow::anyhow!("bcrypt hash error: {e}"))
}

/// Verify a password against a bcrypt hash.
pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    bcrypt::verify(password, hash).map_err(|e| anyhow::anyhow!("bcrypt verify error: {e}"))
}
