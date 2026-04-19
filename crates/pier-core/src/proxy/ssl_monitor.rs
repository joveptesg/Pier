use std::path::Path;
use std::sync::Arc;

use crate::state::AppState;

/// Background task: monitor SSL certificates via Traefik's acme.json.
///
/// Checks periodically, and also when woken via `state.ssl_notify` (e.g.
/// right after a domain is added — so the status transitions from
/// `provisioning` → `active` within seconds instead of minutes):
/// - Parses acme.json for provisioned certificates
/// - Updates domain ssl_status: pending/provisioning → active (when cert found)
/// - Updates ssl_expires_at from certificate data
/// - Marks domains as 'expired' when certificate has expired
pub fn start_ssl_monitor(state: Arc<AppState>) {
    let data_dir = state.config.data_dir.clone();

    tokio::spawn(async move {
        // Initial delay: wait 30s for Traefik to start and provision first certs
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        loop {
            if let Err(e) = check_certificates(&state, &data_dir).await {
                tracing::debug!("SSL monitor: {e}");
            }
            // Poll every 60s, or sooner if woken by a domain add/remove.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                _ = state.ssl_notify.notified() => {}
            }
        }
    });
}

async fn check_certificates(
    state: &Arc<AppState>,
    data_dir: &Path,
) -> anyhow::Result<()> {
    let acme_path = data_dir.join("traefik").join("acme.json");
    if !acme_path.exists() {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&acme_path).await?;
    let acme: serde_json::Value = serde_json::from_str(&content)?;

    // Extract domains that have certificates from acme.json
    // Structure: { "letsencrypt": { "Certificates": [ { "domain": { "main": "..." }, "certificate": "...", "key": "..." } ] } }
    let mut cert_domains: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    if let Some(resolver) = acme.get("letsencrypt") {
        if let Some(certs) = resolver.get("Certificates").and_then(|c| c.as_array()) {
            for cert in certs {
                let main = cert
                    .get("domain")
                    .and_then(|d| d.get("main"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");

                if main.is_empty() {
                    continue;
                }

                // Decode certificate to get expiry date
                let expiry = cert
                    .get("certificate")
                    .and_then(|c| c.as_str())
                    .and_then(parse_cert_expiry);

                let expiry_str = expiry.unwrap_or_default();
                cert_domains.insert(main.to_string(), expiry_str.clone());

                // Also add SANs if present
                if let Some(sans) = cert.get("domain").and_then(|d| d.get("sans")).and_then(|s| s.as_array()) {
                    for san in sans {
                        if let Some(s) = san.as_str() {
                            cert_domains.insert(s.to_string(), expiry_str.clone());
                        }
                    }
                }
            }
        }
    }

    if cert_domains.is_empty() {
        return Ok(());
    }

    // Update domain statuses in DB
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, domain, ssl_status FROM domains",
    )?;

    let domains: Vec<(String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let now = chrono::Utc::now();

    for (id, domain, current_status) in &domains {
        // Strip path prefix for matching (acme.json only has hostnames)
        let hostname = domain.split('/').next().unwrap_or(domain);

        if let Some(expiry_str) = cert_domains.get(hostname) {
            // Certificate exists for this domain
            if current_status == "pending" || current_status == "provisioning" {
                if expiry_str.is_empty() {
                    // Cert found but couldn't parse expiry — mark active anyway
                    let _ = db.execute(
                        "UPDATE domains SET ssl_status = 'active', updated_at = datetime('now') WHERE id = ?1",
                        [id],
                    );
                } else {
                    let _ = db.execute(
                        "UPDATE domains SET ssl_status = 'active', ssl_expires_at = ?2, updated_at = datetime('now') WHERE id = ?1",
                        rusqlite::params![id, expiry_str],
                    );
                }
                tracing::info!("SSL active for {domain}");
            } else if current_status == "active" && !expiry_str.is_empty() {
                // Update expiry and check if expired
                if let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(expiry_str) {
                    let new_status = if expiry < now {
                        "expired"
                    } else if expiry < now + chrono::Duration::days(7) {
                        "expiring"
                    } else {
                        "active"
                    };
                    let _ = db.execute(
                        "UPDATE domains SET ssl_status = ?2, ssl_expires_at = ?3, updated_at = datetime('now') WHERE id = ?1",
                        rusqlite::params![id, new_status, expiry_str],
                    );
                    if new_status != "active" {
                        tracing::warn!("SSL {new_status} for {domain} (expires: {expiry_str})");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Parse a base64-encoded PEM certificate and extract the NotAfter date.
fn parse_cert_expiry(b64_cert: &str) -> Option<String> {
    use base64::Engine;
    let pem_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_cert)
        .ok()?;
    let pem_str = String::from_utf8_lossy(&pem_bytes);

    // Simple PEM parsing: find the base64 block between BEGIN/END CERTIFICATE
    let cert_b64: String = pem_str
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();

    let der = base64::engine::general_purpose::STANDARD
        .decode(cert_b64.trim())
        .ok()?;

    // Parse X.509 NotAfter from DER — look for the second UTCTime or GeneralizedTime
    // This is a simplified parser that works for Let's Encrypt certs
    parse_not_after_from_der(&der)
}

/// Minimal DER parser to extract NotAfter from X.509 certificate.
fn parse_not_after_from_der(der: &[u8]) -> Option<String> {
    // Find UTCTime (tag 0x17) or GeneralizedTime (tag 0x18) entries
    // In X.509, NotBefore is the first time, NotAfter is the second
    let mut times = Vec::new();
    let mut i = 0;
    while i < der.len().saturating_sub(2) {
        if der[i] == 0x17 {
            // UTCTime
            let len = der[i + 1] as usize;
            if i + 2 + len <= der.len() {
                if let Ok(s) = std::str::from_utf8(&der[i + 2..i + 2 + len]) {
                    if let Some(dt) = parse_utc_time(s) {
                        times.push(dt);
                    }
                }
            }
        } else if der[i] == 0x18 {
            // GeneralizedTime
            let len = der[i + 1] as usize;
            if i + 2 + len <= der.len() {
                if let Ok(s) = std::str::from_utf8(&der[i + 2..i + 2 + len]) {
                    if let Some(dt) = parse_generalized_time(s) {
                        times.push(dt);
                    }
                }
            }
        }
        i += 1;
    }

    // NotAfter is the second time field in the Validity sequence
    times.get(1).map(|dt| dt.to_rfc3339())
}

fn parse_utc_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Format: YYMMDDHHMMSSZ
    let s = s.trim_end_matches('Z');
    if s.len() < 12 {
        return None;
    }
    let year: i32 = s[0..2].parse().ok()?;
    let year = if year >= 50 { 1900 + year } else { 2000 + year };
    let month: u32 = s[2..4].parse().ok()?;
    let day: u32 = s[4..6].parse().ok()?;
    let hour: u32 = s[6..8].parse().ok()?;
    let min: u32 = s[8..10].parse().ok()?;
    let sec: u32 = s[10..12].parse().ok()?;

    chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, min, sec)
        .map(|ndt| ndt.and_utc())
}

fn parse_generalized_time(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Format: YYYYMMDDHHMMSSZ
    let s = s.trim_end_matches('Z');
    if s.len() < 14 {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[4..6].parse().ok()?;
    let day: u32 = s[6..8].parse().ok()?;
    let hour: u32 = s[8..10].parse().ok()?;
    let min: u32 = s[10..12].parse().ok()?;
    let sec: u32 = s[12..14].parse().ok()?;

    chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, min, sec)
        .map(|ndt| ndt.and_utc())
}
