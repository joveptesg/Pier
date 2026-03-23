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
