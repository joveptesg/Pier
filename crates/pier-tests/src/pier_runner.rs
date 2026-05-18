//! Spawn an isolated pier-core subprocess for testing.
//!
//! Each runner owns a fresh `tempfile::TempDir` for `PIER_DATA_DIR`, binds
//! to 127.0.0.1:{port} with TLS off, and polls `/health` until the server
//! reports ready. `Drop` kills the child and removes the data dir.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tempfile::TempDir;
use tokio::process::{Child, Command};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PierRunner {
    child: Child,
    data_dir: TempDir,
    port: u16,
    version: String,
}

impl PierRunner {
    pub async fn spawn(pier_bin: &Path, port: u16) -> Result<Self> {
        let data_dir = TempDir::new().context("creating tempdir for PIER_DATA_DIR")?;
        let mut cmd = Command::new(pier_bin);
        cmd.env("PIER_DATA_DIR", data_dir.path())
            .env("PIER_HOST", "127.0.0.1")
            .env("PIER_PORT", port.to_string())
            .env("PIER_TLS_MODE", "off")
            .env("PIER_LOG_LEVEL", "warn")
            // Disable Docker connection — pier prints a warning and runs
            // without proxy auto-start, which is fine for registry tests.
            .env("PIER_DOCKER_HOST", "tcp://127.0.0.1:1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn().context("spawning pier subprocess")?;

        let base = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        let version = wait_for_ready(&client, &base)
            .await
            .context("pier did not become ready in time")?;

        Ok(Self {
            child,
            data_dir,
            port,
            version,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.path().to_path_buf()
    }

    pub fn pier_version(&self) -> &str {
        &self.version
    }
}

impl Drop for PierRunner {
    fn drop(&mut self) {
        // `kill_on_drop(true)` handles termination; explicit start_kill is a
        // belt-and-braces against tokio runtime quirks (e.g. drop on a
        // detached thread that lost its runtime handle).
        let _ = self.child.start_kill();
    }
}

async fn wait_for_ready(client: &reqwest::Client, base: &str) -> Result<String> {
    let started = Instant::now();
    let url = format!("{base}/health");
    let mut last_err: Option<String> = None;
    while started.elapsed() < HEALTH_TIMEOUT {
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                let version = body
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return Ok(version);
            }
            Ok(r) => last_err = Some(format!("status {}", r.status())),
            Err(e) => last_err = Some(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "pier /health never returned 200 within {:?}: last={}",
        HEALTH_TIMEOUT,
        last_err.unwrap_or_else(|| "no attempts".into())
    )
}
