use anyhow::Result;
use bollard::query_parameters::{ListImagesOptions, RemoveImageOptions};
use bollard::Docker;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    pub id: String,
    pub repo_tags: Vec<String>,
    pub size: i64,
    pub created: i64,
}

/// List all Docker images.
pub async fn list_images(docker: &Docker) -> Result<Vec<ImageInfo>> {
    let opts = ListImagesOptions {
        all: false,
        ..Default::default()
    };

    let images = docker.list_images(Some(opts)).await?;

    let result = images
        .into_iter()
        .map(|img| ImageInfo {
            id: img.id.chars().take(19).collect(),
            repo_tags: img.repo_tags,
            size: img.size,
            created: img.created,
        })
        .collect();

    Ok(result)
}

/// Remove a Docker image.
pub async fn remove_image(docker: &Docker, id: &str, force: bool) -> Result<()> {
    docker
        .remove_image(
            id,
            Some(RemoveImageOptions {
                force,
                noprune: false,
                ..Default::default()
            }),
            None,
        )
        .await?;
    Ok(())
}
