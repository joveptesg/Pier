use std::path::Path;

use anyhow::Result;
use tokio::process::Command;

use super::Client;

pub struct Bun;

#[async_trait::async_trait]
impl Client for Bun {
    fn id(&self) -> &'static str {
        "bun"
    }
    fn binary(&self) -> &'static str {
        "bun"
    }
    async fn install(
        &self,
        workdir: &Path,
        package: &str,
        registry_url: &str,
    ) -> Result<std::process::Output> {
        let out = Command::new(self.binary())
            .current_dir(workdir)
            .arg("add")
            .arg(package)
            .arg(format!("--registry={registry_url}"))
            .output()
            .await?;
        Ok(out)
    }
}
