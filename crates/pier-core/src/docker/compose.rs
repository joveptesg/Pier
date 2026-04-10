use anyhow::Result;
use std::path::PathBuf;
use tokio::process::Command;

use crate::config::PierConfig;

/// Base directory for compose stacks.
fn stacks_dir(config: &PierConfig) -> PathBuf {
    config.data_dir.join("stacks")
}

/// Write compose YAML to disk and run `docker compose up -d`.
pub async fn deploy_stack(name: &str, yaml_content: &str, config: &PierConfig) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    tokio::fs::create_dir_all(&stack_dir).await?;

    let compose_file = stack_dir.join("docker-compose.yml");
    tokio::fs::write(&compose_file, yaml_content).await?;

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_file)
        .args(["up", "-d"])
        .current_dir(&stack_dir)
        .env("HOME", config.data_dir.parent().unwrap_or(&config.data_dir))
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    if !output.status.success() {
        anyhow::bail!("docker compose up failed: {combined}");
    }

    Ok(combined)
}

/// Run `docker compose down` for a stack.
pub async fn down_stack(name: &str, config: &PierConfig) -> Result<String> {
    let stack_dir = stacks_dir(config).join(name);
    let compose_file = stack_dir.join("docker-compose.yml");

    if !compose_file.exists() {
        anyhow::bail!("Stack '{name}' not found");
    }

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&compose_file)
        .arg("down")
        .current_dir(&stack_dir)
        .env("HOME", config.data_dir.parent().unwrap_or(&config.data_dir))
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(format!("{stdout}{stderr}"))
}

/// List all stacks by scanning the stacks directory.
#[allow(dead_code)]
pub async fn list_stacks_on_disk(config: &PierConfig) -> Result<Vec<String>> {
    let dir = stacks_dir(config);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut stacks = Vec::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let compose_file = entry.path().join("docker-compose.yml");
            if compose_file.exists() {
                if let Some(name) = entry.file_name().to_str() {
                    stacks.push(name.to_string());
                }
            }
        }
    }

    Ok(stacks)
}

/// Read compose YAML content for a stack.
#[allow(dead_code)]
pub async fn read_stack_yaml(name: &str, config: &PierConfig) -> Result<String> {
    let compose_file = stacks_dir(config).join(name).join("docker-compose.yml");
    Ok(tokio::fs::read_to_string(compose_file).await?)
}

/// Remove stack directory.
pub async fn remove_stack(name: &str, config: &PierConfig) -> Result<()> {
    let stack_dir = stacks_dir(config).join(name);
    if stack_dir.exists() {
        tokio::fs::remove_dir_all(stack_dir).await?;
    }
    Ok(())
}
