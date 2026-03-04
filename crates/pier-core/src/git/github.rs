use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use super::RemoteRepo;

#[derive(Debug, Deserialize)]
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

/// List repos from a GitHub user or organization.
/// base_url should be like "https://github.com/my-org"
pub async fn list_repos(base_url: &str, access_token: Option<&str>) -> Result<Vec<RemoteRepo>> {
    let parts: Vec<&str> = base_url.trim_end_matches('/').rsplit('/').collect();
    let owner = parts.first().copied().unwrap_or("");
    if owner.is_empty() {
        anyhow::bail!("Cannot parse owner from URL: {base_url}");
    }

    let api_url = format!("https://api.github.com/users/{owner}/repos?per_page=100&sort=updated");

    let client = Client::new();
    let mut req = client
        .get(&api_url)
        .header("User-Agent", "Pier-PaaS")
        .header("Accept", "application/vnd.github+json");

    if let Some(token) = access_token {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API error ({status}): {body}");
    }

    let repos: Vec<GhRepo> = resp.json().await?;

    Ok(repos
        .into_iter()
        .map(|r| RemoteRepo {
            name: r.name,
            full_name: r.full_name,
            url: r.html_url,
            clone_url: r.clone_url,
            default_branch: r.default_branch.unwrap_or_else(|| "main".to_string()),
            is_private: r.private,
            description: r.description,
            updated_at: r.updated_at,
        })
        .collect())
}
