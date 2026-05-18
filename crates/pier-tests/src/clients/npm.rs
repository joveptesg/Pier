use std::path::Path;

use anyhow::Result;
use tokio::process::Command;

use super::Client;

pub struct Npm;

#[async_trait::async_trait]
impl Client for Npm {
    fn id(&self) -> &'static str {
        "npm"
    }
    fn binary(&self) -> &'static str {
        "npm"
    }
    async fn install(
        &self,
        workdir: &Path,
        package: &str,
        registry_url: &str,
    ) -> Result<std::process::Output> {
        let out = Command::new(self.binary())
            .current_dir(workdir)
            .arg("install")
            .arg(package)
            .arg("--no-fund")
            .arg("--no-audit")
            .arg(format!("--registry={registry_url}"))
            .arg("--userconfig")
            .arg(workdir.join(".npmrc"))
            .output()
            .await?;
        Ok(out)
    }
}
