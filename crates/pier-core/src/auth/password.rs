use anyhow::Result;

/// Hash a plaintext password with bcrypt (cost=12).
pub fn hash_password(password: &str) -> Result<String> {
    bcrypt::hash(password, 12).map_err(|e| anyhow::anyhow!("bcrypt hash error: {e}"))
}

/// Verify a password against a bcrypt hash.
pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    bcrypt::verify(password, hash).map_err(|e| anyhow::anyhow!("bcrypt verify error: {e}"))
}
