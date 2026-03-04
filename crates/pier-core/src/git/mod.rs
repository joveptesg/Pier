pub mod github;
pub mod github_app;
pub mod gitlab;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRepo {
    pub name: String,
    pub full_name: String,
    pub url: String,
    pub clone_url: String,
    pub default_branch: String,
    pub is_private: bool,
    pub description: Option<String>,
    pub updated_at: Option<String>,
}

/// Fetch repos from a source based on its type.
pub async fn list_repos(
    source_type: &str,
    base_url: &str,
    access_token: Option<&str>,
) -> Result<Vec<RemoteRepo>> {
    match source_type {
        "github" => github::list_repos(base_url, access_token).await,
        "gitlab" => gitlab::list_repos(base_url, access_token).await,
        _ => Ok(Vec::new()),
    }
}
