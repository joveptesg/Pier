//! Fingerprint-pinned HTTPS client for the core→agent channel.
//!
//! `pier-agent` serves its API over TLS with a self-signed certificate.
//! There is no PKI to validate against — agents are reached by raw IPv4/IPv6
//! literal or by their WireGuard mesh IP, none of which a public CA would
//! ever certify. Instead, core **pins the SHA-256 of the agent's leaf
//! certificate** (lowercase hex), learned over the bootstrap-authenticated
//! `/handshake`. Every subsequent call verifies the presented leaf hashes to
//! the pinned value; a mismatch fails the TLS handshake before any request
//! body (token, compose YAML, mesh config) reaches the wire.
//!
//! Hostname/chain validation is deliberately skipped (the verifier ignores
//! `server_name`): the fingerprint *is* the identity. The handshake
//! signature is still verified, so a passive attacker cannot replay a
//! captured certificate without the matching private key.
//!
//! `fingerprint = None` (server row not yet pinned) accepts any leaf for that
//! one call — the narrow pre-enrollment window, before the handshake has
//! delivered a fingerprint. Once pinned, every call is checked.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// SHA-256 of a DER-encoded certificate, lowercase hex. This is the exact
/// value `openssl x509 -outform DER | sha256sum` produces, so the install
/// script and the agent agree with core byte-for-byte.
pub fn fingerprint_hex(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

#[derive(Debug)]
struct PinnedVerifier {
    /// Expected leaf fingerprint (lowercase hex), or `None` to accept any.
    expected: Option<String>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(expected) = &self.expected {
            let got = fingerprint_hex(end_entity.as_ref());
            if !got.eq_ignore_ascii_case(expected) {
                return Err(rustls::Error::General(format!(
                    "agent TLS fingerprint mismatch (pinned {expected}, presented {got})"
                )));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The process-wide rustls crypto provider. `pier-core`'s `main` installs the
/// aws-lc-rs provider as the default at startup; we reuse it so client and
/// panel-server agree on cipher suites. Falls back to a fresh aws-lc-rs
/// provider for unit tests that never call `install_default`.
fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// Build a reqwest client that pins the agent's leaf-cert fingerprint.
///
/// `fingerprint` is the lowercase-hex SHA-256 of the agent's leaf DER
/// (column `servers.agent_tls_fingerprint`); `None` accepts any leaf for the
/// pre-enrollment window. `timeout` mirrors the per-call deadlines the old
/// plaintext clients used.
pub fn build_agent_client(fingerprint: Option<&str>, timeout: Duration) -> Result<reqwest::Client> {
    let provider = provider();
    let verifier = Arc::new(PinnedVerifier {
        expected: fingerprint.map(|s| s.trim().to_ascii_lowercase()),
        provider: provider.clone(),
    });

    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls: safe default protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // The agent speaks HTTP/1.1; pin ALPN so reqwest doesn't offer h2 to a
    // server that won't negotiate it.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    reqwest::Client::builder()
        .timeout(timeout)
        .use_preconfigured_tls(tls)
        .build()
        .context("building pinned agent http client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;

    fn self_signed_der() -> Vec<u8> {
        let key = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        key.cert.der().as_ref().to_vec()
    }

    #[test]
    fn fingerprint_is_lowercase_hex_sha256() {
        let fp = fingerprint_hex(b"hello");
        // sha256("hello")
        assert_eq!(
            fp,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(fp.len(), 64);
    }

    #[test]
    fn verifier_accepts_matching_fingerprint() {
        let der = self_signed_der();
        let expected = fingerprint_hex(&der);
        let v = PinnedVerifier {
            expected: Some(expected),
            provider: provider(),
        };
        let cert = CertificateDer::from(der);
        let r = v.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(r.is_ok(), "matching fingerprint must verify: {r:?}");
    }

    #[test]
    fn verifier_rejects_mismatched_fingerprint() {
        let der = self_signed_der();
        let v = PinnedVerifier {
            expected: Some("00".repeat(32)),
            provider: provider(),
        };
        let cert = CertificateDer::from(der);
        let r = v.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(r.is_err(), "wrong fingerprint must be rejected");
    }

    #[test]
    fn verifier_none_accepts_any() {
        let der = self_signed_der();
        let v = PinnedVerifier {
            expected: None,
            provider: provider(),
        };
        let cert = CertificateDer::from(der);
        let r = v.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("localhost").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(r.is_ok(), "None pin must accept any leaf");
    }
}
