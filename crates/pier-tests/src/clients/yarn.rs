use std::path::Path;

use anyhow::Result;
use tokio::process::Command;

use super::Client;

/// Yarn 1.22.x classic. yarn 4 berry has a wholly different CLI surface
/// and a separate scenario set in the install matrix (P3 follow-up if
/// needed).
pub struct Yarn1;

#[async_trait::async_trait]
impl Client for Yarn1 {
    fn id(&self) -> &'static str {
        "yarn1"
    }
    fn binary(&self) -> &'static str {
        "yarn"
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
            .arg("--registry")
            .arg(registry_url)
            .arg("--no-lockfile")
            // yarn 1 ignores --userconfig; relies on .npmrc in cwd which the
            // harness writes alongside package.json.
            .output()
            .await?;
        Ok(out)
    }
}
