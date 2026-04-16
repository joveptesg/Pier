use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};

/// Get or generate the encryption key from PIER_SECRET env var.
/// Key is 32 bytes (256-bit), base64-encoded in the env var.
pub fn get_secret_key() -> [u8; 32] {
    if let Ok(b64) = std::env::var("PIER_SECRET") {
        if let Ok(bytes) = B64.decode(&b64) {
            if bytes.len() >= 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes[..32]);
                return key;
            }
        }
    }

    // Generate a new key and auto-save to .env
    let key: [u8; 32] = rand::random();
    let b64 = B64.encode(key);

    // Try to append to /opt/pier/.env (or current dir .env)
    let env_paths = ["/opt/pier/.env", ".env"];
    for path in &env_paths {
        let p = std::path::Path::new(path);
        if p.exists() || p.parent().map(|d| d.exists()).unwrap_or(false) {
            let existing = std::fs::read_to_string(p).unwrap_or_default();
            if !existing.contains("PIER_SECRET=") {
                let line = format!("\nPIER_SECRET={b64}\n");
                if std::fs::OpenOptions::new().create(true).append(true).open(p)
                    .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
                    .is_ok()
                {
                    tracing::info!("Generated PIER_SECRET and saved to {path}");
                    std::env::set_var("PIER_SECRET", &b64);
                    return key;
                }
            }
        }
    }

    tracing::warn!("PIER_SECRET generated but could not save to .env. Add manually:\nPIER_SECRET={b64}");
    key
}

/// Encrypt a string. Returns base64-encoded "nonce:ciphertext".
pub fn encrypt(plaintext: &str, key: &[u8; 32]) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| e.to_string())?;

    // Format: base64(nonce):base64(ciphertext)
    Ok(format!("ENC:{}:{}", B64.encode(nonce_bytes), B64.encode(ciphertext)))
}

/// Decrypt a string. Input format: "ENC:base64(nonce):base64(ciphertext)".
/// If input is not encrypted (no ENC: prefix), returns as-is (backward compat).
pub fn decrypt(data: &str, key: &[u8; 32]) -> Result<String, String> {
    if !data.starts_with("ENC:") {
        // Not encrypted — return as-is (backward compatibility with existing data)
        return Ok(data.to_string());
    }

    let parts: Vec<&str> = data.splitn(3, ':').collect();
    if parts.len() != 3 {
        return Err("Invalid encrypted format".to_string());
    }

    let nonce_bytes = B64.decode(parts[1]).map_err(|e| e.to_string())?;
    let ciphertext = B64.decode(parts[2]).map_err(|e| e.to_string())?;

    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length".to_string());
    }

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|e| e.to_string())?;

    String::from_utf8(plaintext).map_err(|e| e.to_string())
}
