//! Pier registry test harness.
//!
//! Spawns an isolated `pier` subprocess against a fresh data dir, bootstraps
//! an admin + npm token via the public API, runs the registry scenario matrix,
//! and emits Markdown + JUnit reports. Designed for CI and ad-hoc VPS runs —
//! replaces the bash smoke script (`pier-smoke-run-v3.sh`).
//!
//! Iteration P1: scaffold + Pier bootstrap + protocol smoke scenarios
//! (ping, whoami, packument/abbreviated/ETag, tarball streaming).
//! Multi-client install matrix and JUnit output land in P2/P3.

mod bootstrap;
mod clients;
mod fixture;
mod pier_runner;
mod report;
mod scenario;
mod scenarios;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::pier_runner::PierRunner;
use crate::report::Report;
use crate::scenario::ScenarioCtx;

#[derive(Parser, Debug)]
#[command(name = "pier-tests", about = "Pier registry test harness")]
struct Args {
    /// Path to the `pier` binary. Defaults to `target/release/pier`
    /// relative to the workspace root.
    #[arg(long)]
    pier_bin: Option<PathBuf>,

    /// Where to write the Markdown report. Defaults to stdout.
    #[arg(long)]
    report_md: Option<PathBuf>,

    /// Where to write the JUnit XML report. Defaults to no JUnit output.
    #[arg(long)]
    report_junit: Option<PathBuf>,

    /// Port the spawned Pier binds to (host = 127.0.0.1, TLS off).
    #[arg(long, default_value_t = 18090)]
    port: u16,

    /// Verbose tracing (debug-level for pier-tests; pier itself still uses
    /// its own RUST_LOG/PIER_LOG_LEVEL).
    #[arg(long)]
    verbose: bool,

    /// Skip teardown — leaves the Pier data dir and process for debugging.
    #[arg(long)]
    keep: bool,

    /// Run against an already-deployed Pier instead of spawning one. Pass the
    /// panel base URL (e.g. `https://test1.devcom.app`). Requires
    /// `--external-token`.
    #[arg(long)]
    external_url: Option<String>,

    /// Bearer token for `--external-url` mode. Mint one in the panel under
    /// Account → Tokens → New.
    #[arg(long)]
    external_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = if args.verbose {
        EnvFilter::new("pier_tests=debug,info")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let report = match (args.external_url.as_deref(), args.external_token.as_deref()) {
        (Some(url), Some(token)) => {
            tracing::info!(%url, "Running against external Pier (no spawn)");
            run_matrix_external(url.trim_end_matches('/'), token).await?
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("--external-url and --external-token must be provided together");
        }
        (None, None) => {
            let pier_bin = resolve_pier_bin(args.pier_bin.as_deref())?;
            tracing::info!(?pier_bin, "Using pier binary");
            let mut runner = PierRunner::spawn(&pier_bin, args.port)
                .await
                .context("spawning Pier subprocess")?;
            tracing::info!(port = args.port, "Pier subprocess up");
            let result = run_matrix(&runner.base_url(), &mut runner).await;
            if args.keep {
                tracing::warn!(
                    data_dir = ?runner.data_dir(),
                    "--keep set: leaving Pier running. Kill manually when done."
                );
                std::mem::forget(runner);
            }
            result?
        }
    };

    if let Some(path) = args.report_md.as_deref() {
        std::fs::write(path, report.to_markdown())?;
        tracing::info!(?path, "Markdown report written");
    } else {
        println!("{}", report.to_markdown());
    }

    if let Some(path) = args.report_junit.as_deref() {
        std::fs::write(path, report.to_junit())?;
        tracing::info!(?path, "JUnit report written");
    }

    if report.failed() > 0 {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_matrix(base_url: &str, runner: &mut PierRunner) -> Result<Report> {
    let bootstrap = bootstrap::bootstrap_admin_and_token(base_url)
        .await
        .context("bootstrap admin + npm token")?;
    tracing::info!(token_prefix = %&bootstrap.token[..16], "npm token issued");

    let ctx = ScenarioCtx {
        base_url: base_url.to_string(),
        registry_url: format!("{base_url}/registry/npm/"),
        token: bootstrap.token.clone(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
    };

    let mut report = Report::new("pier-tests", runner.pier_version().to_string());
    for scenario in scenarios::all() {
        report.push(scenario.run(&ctx).await);
    }
    Ok(report)
}

async fn run_matrix_external(base_url: &str, token: &str) -> Result<Report> {
    let ctx = ScenarioCtx {
        base_url: base_url.to_string(),
        registry_url: format!("{base_url}/registry/npm/"),
        token: token.to_string(),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
    };
    // Probe `<base>/health` for the pier version. Best-effort — unknown is
    // fine, the report stays useful without it.
    let version = ctx
        .http
        .get(format!("{base_url}/health"))
        .send()
        .await
        .ok()
        .and_then(|r| {
            if r.status().is_success() {
                Some(r)
            } else {
                None
            }
        })
        .map(|r| async move { r.json::<serde_json::Value>().await.ok() });
    let version = match version {
        Some(fut) => fut
            .await
            .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_string()),
        None => "unknown".to_string(),
    };

    let mut report = Report::new("pier-tests (external)", version);
    for scenario in scenarios::all() {
        report.push(scenario.run(&ctx).await);
    }
    Ok(report)
}

fn resolve_pier_bin(explicit: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if !p.exists() {
            anyhow::bail!("pier binary not found at {}", p.display());
        }
        return Ok(p.to_path_buf());
    }
    let workspace = workspace_root()?;
    for candidate in ["target/release/pier", "target/debug/pier"] {
        let p = workspace.join(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "could not locate pier binary under {}/target/. Build it with `cargo build --release -p pier-core` or pass --pier-bin.",
        workspace.display()
    );
}

fn workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            anyhow::bail!("workspace root not found above current directory");
        }
    }
}
