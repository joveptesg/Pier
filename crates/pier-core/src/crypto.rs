use std::sync::OnceLock;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};

static SECRET_KEY: OnceLock<[u8; 32]> = OnceLock::new();

const SECRET_FILENAME: &str = ".pier-secret";

/// Return the process-wide AES-256 key. Computed once, cached for the lifetime
/// of the process — all `encrypt`/`decrypt` calls receive the same bytes.
///
/// Resolution order:
/// 1. `PIER_SECRET` env var (populated by systemd `EnvironmentFile=/opt/pier/.env`).
/// 2. `{PIER_DATA_DIR}/.pier-secret` — written by a prior run of the service.
/// 3. Generate a fresh random 32-byte key and persist it to `{PIER_DATA_DIR}/.pier-secret`.
///    If that write fails we **panic** at startup — it is much safer to refuse
///    to run than to silently corrupt every subsequent encrypt with a key that
///    nobody will ever see again.
pub fn get_secret_key() -> [u8; 32] {
    *SECRET_KEY.get_or_init(load_or_generate_key)
}

fn load_or_generate_key() -> [u8; 32] {
    if let Some(key) = read_env_secret() {
        tracing::debug!("PIER_SECRET loaded from environment");
        return key;
    }

    if let Some(key) = read_data_dir_secret() {
        tracing::debug!("PIER_SECRET loaded from data dir");
        // Mirror it into env so any child-process helpers see the same value.
        let _ = std::env::var("PIER_SECRET");
        return key;
    }

    let key: [u8; 32] = rand::random();
    let b64 = B64.encode(key);
    match persist_to_data_dir(&b64) {
        Ok(path) => tracing::info!("Generated PIER_SECRET and saved to {path}"),
        Err(e) => panic!(
            "PIER_SECRET cannot be persisted (tried data_dir). \
             Fix the data_dir permissions or set PIER_SECRET in /opt/pier/.env. \
             Underlying error: {e}"
        ),
    }
    std::env::set_var("PIER_SECRET", &b64);
    key
}

fn read_env_secret() -> Option<[u8; 32]> {
    let b64 = std::env::var("PIER_SECRET").ok()?;
    decode_key(&b64)
}

fn read_data_dir_secret() -> Option<[u8; 32]> {
    let path = data_dir_secret_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    let key = decode_key(contents.trim())?;
    // Also surface it via the env for anything inside this process that may
    // call `std::env::var("PIER_SECRET")` directly.
    if std::env::var("PIER_SECRET").is_err() {
        std::env::set_var("PIER_SECRET", contents.trim());
    }
    Some(key)
}

fn persist_to_data_dir(b64: &str) -> std::io::Result<String> {
    let path = data_dir_secret_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, b64)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path.display().to_string())
}

fn data_dir_secret_path() -> std::path::PathBuf {
    let data_dir = std::env::var("PIER_DATA_DIR").unwrap_or_else(|_| "./data".into());
    std::path::Path::new(&data_dir).join(SECRET_FILENAME)
}

fn decode_key(b64: &str) -> Option<[u8; 32]> {
    let bytes = B64.decode(b64.trim()).ok()?;
    if bytes.len() < 32 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes[..32]);
    Some(key)
}

/// Encrypt a string. Returns `"ENC:base64(nonce):base64(ciphertext)"`.
pub fn encrypt(plaintext: &str, key: &[u8; 32]) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| e.to_string())?;

    Ok(format!(
        "ENC:{}:{}",
        B64.encode(nonce_bytes),
        B64.encode(ciphertext)
    ))
}

/// Decrypt a string of the form `"ENC:base64(nonce):base64(ciphertext)"`.
/// Strings without the `ENC:` prefix are returned as-is for backward compatibility.
pub fn decrypt(data: &str, key: &[u8; 32]) -> Result<String, String> {
    if !data.starts_with("ENC:") {
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

/// Encrypt a JSON string for storage in `services.env_json`.
/// Falls back to storing the plaintext if encryption fails (shouldn't happen
/// with a valid key, but we never want to lose user data silently).
pub fn encrypt_env_json(plain: &str) -> String {
    let key = get_secret_key();
    match encrypt(plain, &key) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("encrypt_env_json failed ({e}); storing plaintext as a fallback");
            plain.to_string()
        }
    }
}

/// Decrypt a stored `env_json` value. Returns `"{}"` on any failure so the
/// UI and downstream JSON parsers never see garbage.
///
/// Handles three cases:
/// - `Some(plaintext JSON)` — backward compatible, returned as-is if valid JSON.
/// - `Some("ENC:...")` — decrypted with the current key.
/// - `None` / empty / `"null"` / corrupted — returns `"{}"`.
pub fn decrypt_env_json(stored: Option<&str>) -> String {
    let s = match stored {
        Some(s) if !s.is_empty() && s != "null" => s,
        _ => return "{}".into(),
    };

    if !s.starts_with("ENC:") {
        if serde_json::from_str::<serde_json::Value>(s).is_ok() {
            return s.to_string();
        }
        return "{}".into();
    }

    let key = get_secret_key();
    match decrypt(s, &key) {
        Ok(p) if serde_json::from_str::<serde_json::Value>(&p).is_ok() => p,
        _ => {
            tracing::warn!("env_json decrypt failed with current key — data may need recovery");
            "{}".into()
        }
    }
}

/// Run at startup — if this fails the key isn't usable and we must not
/// continue, otherwise we'll corrupt every user interaction.
pub fn self_check() {
    let key = get_secret_key();
    let enc = encrypt("canary", &key).expect("canary encrypt");
    let dec = decrypt(&enc, &key).expect("canary decrypt");
    assert_eq!(dec, "canary", "PIER_SECRET self-check mismatch");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_key_across_calls() {
        let k1 = get_secret_key();
        let k2 = get_secret_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn roundtrip_env_json() {
        let plain = r#"{"FOO":"bar"}"#;
        let enc = encrypt_env_json(plain);
        assert!(enc.starts_with("ENC:"));
        let dec = decrypt_env_json(Some(&enc));
        assert_eq!(dec, plain);
    }

    #[test]
    fn decrypt_env_json_plaintext_passthrough() {
        assert_eq!(decrypt_env_json(Some(r#"{"A":"1"}"#)), r#"{"A":"1"}"#);
    }

    #[test]
    fn decrypt_env_json_handles_none_and_empty() {
        assert_eq!(decrypt_env_json(None), "{}");
        assert_eq!(decrypt_env_json(Some("")), "{}");
        assert_eq!(decrypt_env_json(Some("null")), "{}");
    }

    #[test]
    fn decrypt_env_json_handles_garbage() {
        assert_eq!(decrypt_env_json(Some("ENC:garbage:nope")), "{}");
        assert_eq!(decrypt_env_json(Some("not json")), "{}");
    }

    #[test]
    fn explicit_key_roundtrip() {
        let key = [42u8; 32];
        let enc = encrypt("hello", &key).unwrap();
        assert!(enc.starts_with("ENC:"));
        assert_eq!(decrypt(&enc, &key).unwrap(), "hello");
        // Wrong key should fail the MAC, not return garbage.
        let wrong = [43u8; 32];
        assert!(decrypt(&enc, &wrong).is_err());
    }

    #[test]
    fn decrypt_plaintext_no_prefix() {
        let key = [1u8; 32];
        // Without ENC: prefix we pass through — backward compat.
        assert_eq!(decrypt("plain text", &key).unwrap(), "plain text");
    }
}
