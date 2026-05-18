//! Bootstrap an admin user + npm token via Pier's public HTTP API.
//!
//! Equivalent of running `/setup` in the UI, then `Account → Tokens → New`.
//! Returns the plaintext `pier_npm_…` token used by the registry scenarios.

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::json;

const ADMIN_USERNAME: &str = "tester";
const ADMIN_EMAIL: &str = "tester@pier-tests.local";
// `validate_password_strength` requires a non-trivial password — this passes
// zxcvbn while staying static for reproducible test runs.
const ADMIN_PASSWORD: &str = "PierTest!Strong#Pass-2026";
const TOKEN_NAME: &str = "pier-tests";

pub struct BootstrapResult {
    pub token: String,
}

pub async fn bootstrap_admin_and_token(base_url: &str) -> Result<BootstrapResult> {
    let client = Client::builder()
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // 1. /api/v1/auth/setup — create the first admin. A fresh tempdir has no
    // `.setup_token`, so the endpoint is unauthenticated (per the comment in
    // `auth::setup_token::SetupTokenStore::load`).
    let resp = client
        .post(format!("{base_url}/api/v1/auth/setup"))
        .json(&json!({
            "username": ADMIN_USERNAME,
            "email":    ADMIN_EMAIL,
            "password": ADMIN_PASSWORD,
        }))
        .send()
        .await
        .context("POST /auth/setup")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("setup failed: {status} — {body}");
    }

    // 2. /api/v1/auth/login — session cookie is captured by the cookie store.
    let resp = client
        .post(format!("{base_url}/api/v1/auth/login"))
        .json(&json!({
            "username": ADMIN_USERNAME,
            "password": ADMIN_PASSWORD,
        }))
        .send()
        .await
        .context("POST /auth/login")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("login failed: {status} — {body}");
    }

    // 3. /api/v1/account/tokens — mint a new api_token. The plaintext is only
    // returned in this response; subsequent reads expose just the prefix.
    let resp = client
        .post(format!("{base_url}/api/v1/account/tokens"))
        .json(&json!({ "name": TOKEN_NAME }))
        .send()
        .await
        .context("POST /account/tokens")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status != StatusCode::OK && status != StatusCode::CREATED {
        anyhow::bail!("token mint failed: {status} — {body}");
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parsing token response: {body}"))?;
    let token = v
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("no `token` field in response: {body}"))?
        .to_string();

    Ok(BootstrapResult { token })
}
