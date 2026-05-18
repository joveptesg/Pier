//! Real-client install shell-outs.
//!
//! Each `Client` impl knows how to install a package into a fresh tempdir.
//! The harness probes each binary (via `--version`) and silently skips
//! clients that aren't on PATH — letting CI / dev workstations run a partial
//! matrix without failing red.

use std::path::Path;

use anyhow::Result;
use tokio::process::Command;

mod bun;
mod npm;
mod pnpm;
mod yarn;

pub fn all() -> Vec<Box<dyn Client>> {
    vec![
        Box::new(npm::Npm),
        Box::new(yarn::Yarn1),
        Box::new(pnpm::Pnpm),
        Box::new(bun::Bun),
    ]
}

#[async_trait::async_trait]
pub trait Client: Send + Sync {
    /// Stable identifier used in scenario names + JUnit testcase.
    fn id(&self) -> &'static str;
    /// Executable name (or absolute path) probed via `--version`.
    fn binary(&self) -> &'static str;
    /// Install `package` inside `workdir`, which already has `.npmrc` + a
    /// stub `package.json` written by the harness.
    async fn install(
        &self,
        workdir: &Path,
        package: &str,
        registry_url: &str,
    ) -> Result<std::process::Output>;
}

/// Probe the binary by running `<binary> --version` once. Returns the version
/// string (trimmed) or `None` if the binary isn't on PATH / produced a
/// non-zero exit.
pub async fn probe(binary: &str) -> Option<String> {
    let out = Command::new(binary).arg("--version").output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
