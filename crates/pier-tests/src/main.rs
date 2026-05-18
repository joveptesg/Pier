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

    let report = result?;

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

    let mut report = Report::new("pier-tests P1", runner.pier_version().to_string());
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
