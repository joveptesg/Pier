//! Docker resource pruning. Extracted from the legacy `main.rs` cleanup
//! `tokio::spawn` loop so the unified scheduler can drive it on cron.
//!
//! The shape is intentionally narrow: callers supply flags, the function
//! shells out to `docker prune`, fires the existing alert hooks, and
//! returns a single-line outcome string for `schedule_runs.output`.

use crate::alerts;
use crate::state::SharedState;

#[derive(Clone, Debug)]
pub struct CleanupOptions {
    pub prune_images: bool,
    pub prune_build_cache: bool,
    pub prune_containers: bool,
    /// Prune the Railpack/BuildKit layer cache (lives inside the
    /// moby/buildkit container, not the host Docker daemon). Conservative
    /// parameters match the safety-net loop in `main.rs`: ~10 GB / 7-day
    /// retention. Operators wanting an aggressive "wipe now" use the
    /// manual Clean button (POST /system/cleanup with target
    /// `railpack_buildkit_cache`), which uses 0/0 instead.
    pub prune_railpack_buildkit: bool,
}

impl CleanupOptions {
    /// Default policy (matches the legacy loop's behaviour from
    /// `settings.cleanup.*`): images + cache yes, containers no,
    /// railpack off (the standalone daily loop in `main.rs` handles
    /// it; turning this on means the operator wants the scheduled
    /// run to do it too).
    pub fn defaults() -> Self {
        Self {
            prune_images: true,
            prune_build_cache: true,
            prune_containers: false,
            prune_railpack_buildkit: false,
        }
    }
}

/// Run the configured prune passes once. Fires `docker_cleanup_success`
/// per-pass on success and `docker_cleanup_failure` on shell errors, same
/// as the legacy loop did, so existing alert rules keep working.
pub async fn run_once(state: &SharedState, opts: &CleanupOptions) -> anyhow::Result<String> {
    let mut summary: Vec<String> = Vec::new();

    if opts.prune_images {
        summary.push(prune_pass(state, "images", &["image", "prune", "-f"]).await);
    }
    if opts.prune_build_cache {
        summary.push(prune_pass(state, "build_cache", &["builder", "prune", "-f"]).await);
    }
    if opts.prune_containers {
        summary.push(prune_pass(state, "containers", &["container", "prune", "-f"]).await);
    }
    if opts.prune_railpack_buildkit {
        // Same numbers as the standalone safety-net loop in main.rs so the
        // two converge on the same target rather than fighting. Idempotent:
        // running both in one day just means one is a no-op.
        summary.push(
            prune_pass(
                state,
                "railpack_buildkit",
                &[
                    "exec",
                    "buildkit",
                    "buildctl",
                    "prune",
                    "--keep-storage",
                    "10737418240",
                    "--keep-duration",
                    "168h",
                ],
            )
            .await,
        );
    }

    if summary.is_empty() {
        return Ok("cleanup: no targets selected".to_string());
    }
    Ok(summary.join(" | "))
}

async fn prune_pass(state: &SharedState, name: &'static str, args: &[&str]) -> String {
    match tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            tracing::info!("Cleanup {name}: {stdout}");
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_success",
                None,
                format!("Docker {name} pruned: {stdout}"),
            )
            .await;
            format!("{name}=ok ({} chars)", stdout.len())
        }
        Err(e) => {
            tracing::warn!("Cleanup {name} failed: {e}");
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_failure",
                None,
                format!("Docker {name} prune failed: {e}"),
            )
            .await;
            format!("{name}=error ({e})")
        }
    }
}
