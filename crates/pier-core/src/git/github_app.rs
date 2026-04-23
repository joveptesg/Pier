use anyhow::{anyhow, Result};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use super::RemoteRepo;

#[derive(Debug, Serialize)]
struct JwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

/// Create a JWT for GitHub App authentication.
fn create_jwt(app_id: &str, private_key: &str) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let claims = JwtClaims {
        iat: now - 60,
        exp: now + 600, // 10 minutes
        iss: app_id.to_string(),
    };

    let key = EncodingKey::from_rsa_pem(private_key.as_bytes())
        .map_err(|e| anyhow!("Invalid RSA private key: {e}"))?;

    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| anyhow!("JWT encode error: {e}"))
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
}

/// Get an installation access token from GitHub.
pub async fn get_installation_token(
    app_id: &str,
    installation_id: i64,
    private_key: &str,
) -> Result<String> {
    let jwt = create_jwt(app_id, private_key)?;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "https://api.github.com/app/installations/{installation_id}/access_tokens"
        ))
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Pier-PaaS")
        .send()
        .await
        .map_err(|e| anyhow!("GitHub API request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub API error {status}: {body}"));
    }

    let data: InstallationTokenResponse = resp.json().await?;
    Ok(data.token)
}

#[derive(Deserialize)]
struct RepoListResponse {
    repositories: Vec<GhRepo>,
}

#[derive(Deserialize)]
struct GhRepo {
    name: String,
    full_name: String,
    html_url: String,
    clone_url: String,
    default_branch: Option<String>,
    private: bool,
    description: Option<String>,
    updated_at: Option<String>,
}

/// List repositories accessible by the GitHub App installation.
pub async fn list_repos(
    app_id: &str,
    installation_id: i64,
    private_key: &str,
) -> Result<Vec<RemoteRepo>> {
    let token = get_installation_token(app_id, installation_id, private_key).await?;

    let client = reqwest::Client::new();
    let mut repos = Vec::new();
    let mut page = 1u32;

    loop {
        let resp = client
            .get("https://api.github.com/installation/repositories")
            .query(&[("per_page", "100"), ("page", &page.to_string())])
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "Pier-PaaS")
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub list repos error: {body}"));
        }

        let data: RepoListResponse = resp.json().await?;
        if data.repositories.is_empty() {
            break;
        }

        for r in data.repositories {
            repos.push(RemoteRepo {
                name: r.name,
                full_name: r.full_name,
                url: r.html_url,
                clone_url: r.clone_url,
                default_branch: r.default_branch.unwrap_or_else(|| "main".to_string()),
                is_private: r.private,
                description: r.description,
                updated_at: r.updated_at,
            });
        }

        page += 1;
        if page > 10 {
            break; // safety limit
        }
    }

    Ok(repos)
}

/// List branches for a repository using GitHub App installation token.
/// Get file content from a GitHub repo via API.
pub async fn get_file_content(
    app_id: &str,
    installation_id: i64,
    private_key: &str,
    repo_full_name: &str,
    branch: &str,
    file_path: &str,
) -> Result<String> {
    let token = get_installation_token(app_id, installation_id, private_key).await?;
    let client = reqwest::Client::new();

    let path = file_path.trim_start_matches('/');
    let resp = client
        .get(format!(
            "https://api.github.com/repos/{repo_full_name}/contents/{path}?ref={branch}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github.raw+json")
        .header("User-Agent", "Pier-PaaS")
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub get file error ({status}): {body}"));
    }

    Ok(resp.text().await?)
}

pub async fn list_branches(
    app_id: &str,
    installation_id: i64,
    private_key: &str,
    repo_full_name: &str,
) -> Result<Vec<String>> {
    let token = get_installation_token(app_id, installation_id, private_key).await?;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!(
            "https://api.github.com/repos/{repo_full_name}/branches?per_page=100"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Pier-PaaS")
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub list branches error: {body}"));
    }

    let data: Vec<serde_json::Value> = resp.json().await?;
    let branches: Vec<String> = data
        .iter()
        .filter_map(|b| b["name"].as_str().map(|s| s.to_string()))
        .collect();

    Ok(branches)
}

/// Exchange a GitHub App Manifest code for app credentials.
/// Called after user creates an app via the manifest flow.
/// Returns: (app_id, slug, pem, webhook_secret, client_id, client_secret, owner_login)
pub async fn exchange_manifest_code(code: &str) -> Result<ManifestExchangeResult> {
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/app-manifests/{code}/conversions");

    let resp = client
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Pier-PaaS")
        .send()
        .await?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub manifest exchange failed: {body}"));
    }

    let data: serde_json::Value = resp.json().await?;

    Ok(ManifestExchangeResult {
        app_id: data["id"].as_i64().unwrap_or(0).to_string(),
        slug: data["slug"].as_str().unwrap_or("").to_string(),
        pem: data["pem"].as_str().unwrap_or("").to_string(),
        webhook_secret: data["webhook_secret"].as_str().unwrap_or("").to_string(),
        client_id: data["client_id"].as_str().unwrap_or("").to_string(),
        client_secret: data["client_secret"].as_str().unwrap_or("").to_string(),
        owner_login: data["owner"]["login"].as_str().unwrap_or("").to_string(),
        html_url: data["html_url"].as_str().unwrap_or("").to_string(),
    })
}

/// Generate GitHub App manifest JSON for the manifest creation flow.
pub fn generate_manifest(pier_url: &str, app_name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": app_name,
        "url": pier_url,
        "hook_attributes": {
            "url": format!("{pier_url}/api/v1/webhooks/github"),
            "active": true
        },
        "redirect_url": format!("{pier_url}/api/v1/sources/github/callback"),
        "callback_urls": [format!("{pier_url}/api/v1/sources/github/callback")],
        "setup_url": format!("{pier_url}/sources"),
        "public": false,
        "default_permissions": {
            "contents": "read",
            "metadata": "read",
            "pull_requests": "read"
        },
        "default_events": ["push"]
    })
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ManifestExchangeResult {
    pub app_id: String,
    pub slug: String,
    pub pem: String,
    pub webhook_secret: String,
    pub client_id: String,
    pub client_secret: String,
    pub owner_login: String,
    pub html_url: String,
}
