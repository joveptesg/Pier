use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;

use super::RemoteRepo;

#[derive(Debug, Deserialize)]
struct GlProject {
    name: String,
    path_with_namespace: String,
    web_url: String,
    http_url_to_repo: String,
    default_branch: Option<String>,
    visibility: String,
    description: Option<String>,
    last_activity_at: Option<String>,
}

/// List repos from a GitLab instance.
/// base_url should be like "https://gitlab.com/my-group"
pub async fn list_repos(base_url: &str, access_token: Option<&str>) -> Result<Vec<RemoteRepo>> {
    let parsed = url::Url::parse(base_url)?;
    let path = parsed.path().trim_matches('/');
    let api_base = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or("gitlab.com")
    );

    let api_url = format!(
        "{api_base}/api/v4/groups/{}/projects?per_page=100&order_by=updated_at",
        urlencoding::encode(path),
    );

    let client = Client::new();
    let mut req = client.get(&api_url);

    if let Some(token) = access_token {
        if !token.is_empty() {
            req = req.header("PRIVATE-TOKEN", token);
        }
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        // Try as user instead of group
        let api_url = format!(
            "{api_base}/api/v4/users/{}/projects?per_page=100&order_by=updated_at",
            urlencoding::encode(path),
        );
        let mut req = client.get(&api_url);
        if let Some(token) = access_token {
            if !token.is_empty() {
                req = req.header("PRIVATE-TOKEN", token);
            }
        }
        let resp = req.send().await?;
        let projects: Vec<GlProject> = resp.json().await?;
        return Ok(convert(projects));
    }

    let projects: Vec<GlProject> = resp.json().await?;
    Ok(convert(projects))
}

fn convert(projects: Vec<GlProject>) -> Vec<RemoteRepo> {
    projects
        .into_iter()
        .map(|p| RemoteRepo {
            name: p.name,
            full_name: p.path_with_namespace,
            url: p.web_url,
            clone_url: p.http_url_to_repo,
            default_branch: p.default_branch.unwrap_or_else(|| "main".to_string()),
            is_private: p.visibility != "public",
            description: p.description,
            updated_at: p.last_activity_at,
        })
        .collect()
}
