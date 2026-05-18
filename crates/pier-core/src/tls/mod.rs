//! Panel TLS termination.
//!
//! The admin panel listens on `:8443` and previously served plain HTTP — meaning
//! the admin password and session cookie traveled in cleartext. This module wires
//! up rustls with an auto-generated self-signed certificate so the listener is
//! always encrypted, even on a fresh VPS where the operator has not yet pointed
//! a domain at the host. ACME / Let's Encrypt is handled in a follow-up (see
//! plan: Phase B).

use std::fs;
use std::net::{IpAddr, UdpSocket};
use std::path::Path;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{generate_simple_self_signed, CertifiedKey};

use crate::config::PierConfig;

const CERT_FILE: &str = "cert.pem";
const KEY_FILE: &str = "key.pem";

/// Load the panel TLS cert/key, generating a self-signed pair on first run.
pub async fn load_or_generate_cert(cfg: &PierConfig) -> Result<RustlsConfig> {
    let cert_path = cfg.tls_cert_dir.join(CERT_FILE);
    let key_path = cfg.tls_cert_dir.join(KEY_FILE);

    if !cert_path.exists() || !key_path.exists() {
        fs::create_dir_all(&cfg.tls_cert_dir)
            .with_context(|| format!("create TLS cert dir {}", cfg.tls_cert_dir.display()))?;
        generate_self_signed(cfg, &cert_path, &key_path)
            .context("generate self-signed panel cert")?;
        tracing::info!(
            "Generated self-signed panel TLS cert at {}",
            cert_path.display()
        );
    }

    RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .with_context(|| {
            format!(
                "load panel TLS material from {} / {}",
                cert_path.display(),
                key_path.display()
            )
        })
}

fn generate_self_signed(cfg: &PierConfig, cert_path: &Path, key_path: &Path) -> Result<()> {
    let mut sans: Vec<String> = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(domain) = &cfg.panel_domain {
        if !sans.iter().any(|s| s == domain) {
            sans.push(domain.clone());
        }
    }
    // Try to learn the host's primary outbound IP — both v4 and v6 —
    // so a peer connecting by raw IP literal passes TLS hostname
    // validation. Best-effort: a host with no v4 (resp. v6) default
    // route simply contributes one fewer SAN.
    if let Some(ip) = primary_outbound_ip_v4() {
        let s = ip.to_string();
        if !sans.iter().any(|x| x == &s) {
            sans.push(s);
        }
    }
    if let Some(ip) = primary_outbound_ip_v6() {
        let s = ip.to_string();
        if !sans.iter().any(|x| x == &s) {
            sans.push(s);
        }
    }

    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(sans)?;
    write_secret(key_path, signing_key.serialize_pem().as_bytes())?;
    fs::write(cert_path, cert.pem()).with_context(|| format!("write {}", cert_path.display()))?;
    Ok(())
}

fn primary_outbound_ip_v4() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

fn primary_outbound_ip_v6() -> Option<IpAddr> {
    let sock = UdpSocket::bind("[::]:0").ok()?;
    // 2606:4700:4700::1111 = Cloudflare DNS over IPv6. The UDP connect
    // doesn't actually transmit packets — it's used to ask the kernel
    // which source address it would pick for the v6 default route.
    sock.connect("[2606:4700:4700::1111]:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// Connecting a UDP socket to a public IP doesn't send any packets; it just
/// forces the kernel to pick the source IP of the default route. That IP is
/// what an external client (or Let's Encrypt) will see, so it's the right
/// thing to put in the cert SAN. Both v4 and v6 variants live above as
/// `primary_outbound_ip_v4` / `primary_outbound_ip_v6`.

#[cfg(unix)]
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    std::io::Write::write_all(&mut f, bytes)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}
