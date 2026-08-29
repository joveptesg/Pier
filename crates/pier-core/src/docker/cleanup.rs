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
    /// Remove tagged images nothing references any more — see
    /// [`super::image_gc`]. Distinct from `prune_images`, which is
    /// `docker image prune -f` and only ever touches untagged layers.
    pub prune_orphan_images: bool,
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
    ///
    /// Orphan images are off too. They are the largest reclaim available on
    /// a long-lived host, but deleting a tagged image is not something a
    /// fresh install should start doing unattended.
    pub fn defaults() -> Self {
        Self {
            prune_images: true,
            prune_build_cache: true,
            prune_containers: false,
            prune_orphan_images: false,
            prune_railpack_buildkit: false,
        }
    }
}

/// Run the configured prune passes once. Fires `docker_cleanup_success`
/// per-pass on success and `docker_cleanup_failure` on failure, same
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
    if opts.prune_orphan_images {
        summary.push(orphan_pass(state).await);
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

/// One `docker ... prune` invocation.
///
/// A non-zero exit is a failure. That sounds obvious, but this used to match
/// only on whether the process spawned, so a prune rejected by the daemon
/// still logged an empty success line and fired `docker_cleanup_success` —
/// a schedule could stay green forever while reclaiming nothing.
async fn prune_pass(state: &SharedState, name: &'static str, args: &[&str]) -> String {
    match super::docker_cmd(state).args(args).output().await {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let reclaimed = parse_reclaimed(&stdout);
            tracing::info!("Cleanup {name}: {stdout}");
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_success",
                None,
                format!("Docker {name} pruned: {stdout}"),
            )
            .await;
            format!("{name}=ok (reclaimed {reclaimed} bytes)")
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            tracing::warn!("Cleanup {name} exited {}: {stderr}", out.status);
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_failure",
                None,
                format!("Docker {name} prune failed: {stderr}"),
            )
            .await;
            format!("{name}=error ({stderr})")
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

/// Scheduled counterpart of the Unused Images button.
async fn orphan_pass(state: &SharedState) -> String {
    match super::image_gc::remove_orphan_images(state).await {
        Ok(s) if s.errors.is_empty() => {
            tracing::info!(
                "Cleanup orphan_images: removed {}, reclaimed {} bytes",
                s.removed,
                s.reclaimed
            );
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_success",
                None,
                format!(
                    "Docker orphan images removed: {} images, {} bytes",
                    s.removed, s.reclaimed
                ),
            )
            .await;
            format!(
                "orphan_images=ok (removed {}, reclaimed {} bytes)",
                s.removed, s.reclaimed
            )
        }
        Ok(s) => {
            // Some images went, some did not. Worth an alert either way: a
            // refusal here means the protection rules and the daemon
            // disagree about what is still in use.
            let detail = s.errors.join("; ");
            tracing::warn!("Cleanup orphan_images partially failed: {detail}");
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_failure",
                None,
                format!("Docker orphan image removal partially failed: {detail}"),
            )
            .await;
            format!(
                "orphan_images=partial (removed {}, {} errors)",
                s.removed,
                s.errors.len()
            )
        }
        Err(e) => {
            tracing::warn!("Cleanup orphan_images failed: {e}");
            alerts::hooks::fire_event(
                state,
                "docker_cleanup_failure",
                None,
                format!("Docker orphan image removal failed: {e}"),
            )
            .await;
            format!("orphan_images=error ({e})")
        }
    }
}

/// Bytes freed, from the `Total reclaimed space: 6.2GB` line every
/// `docker ... prune` ends with.
///
/// Returns 0 when the line is absent — `buildctl prune` prints a table
/// instead — which reads as "nothing reported", not as an error.
pub fn parse_reclaimed(stdout: &str) -> u64 {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("Total reclaimed space:"))
        .map(|size| parse_docker_size(size.trim()))
        .unwrap_or(0)
}

/// Parse a size string as printed by the Docker CLI into bytes.
///
/// `docker system df` formats with Go's `units.HumanSize`, which is
/// **decimal** (`kB`/`MB`/`GB` are powers of 1000). `buildctl` and friends
/// print **binary** suffixes (`KiB`/`MiB`/`GiB`, powers of 1024). Both are
/// accepted, because both reach this function.
///
/// The multiplication happens in `f64` and only the result is rounded. Doing
/// it the other way round — as this did until the Cleanup panel was found to
/// be lying — truncated `"7.647GB"` to `7 GB` and `"35.76MB"` to `35 MB`,
/// which is why every figure in the panel used to look suspiciously round.
pub fn parse_docker_size(s: &str) -> u64 {
    // Longest suffix first: `GiB` must be tried before `B`, `kB` before `B`.
    const UNITS: &[(&str, f64)] = &[
        ("PiB", 1_125_899_906_842_624.0),
        ("TiB", 1_099_511_627_776.0),
        ("GiB", 1_073_741_824.0),
        ("MiB", 1_048_576.0),
        ("KiB", 1024.0),
        ("PB", 1e15),
        ("TB", 1e12),
        ("GB", 1e9),
        ("MB", 1e6),
        ("kB", 1e3),
        ("KB", 1e3),
        ("B", 1.0),
    ];

    let s = s.trim();
    if s.is_empty() {
        return 0;
    }

    for (suffix, unit) in UNITS {
        if let Some(rest) = s.strip_suffix(suffix) {
            let n: f64 = rest.trim().parse().unwrap_or(0.0);
            return if n > 0.0 {
                (n * unit).round() as u64
            } else {
                0
            };
        }
    }

    // No recognised suffix — Docker prints bare byte counts in a few places.
    s.parse::<f64>()
        .map(|n| if n > 0.0 { n.round() as u64 } else { 0 })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{parse_docker_size, parse_reclaimed};

    #[test]
    fn parses_decimal_suffixes() {
        // The three figures the production Cleanup panel was misreporting as
        // 7.0 GB / 1.0 GB / 35.0 MB.
        assert_eq!(parse_docker_size("7.647GB"), 7_647_000_000);
        assert_eq!(parse_docker_size("1.446GB"), 1_446_000_000);
        assert_eq!(parse_docker_size("35.76MB"), 35_760_000);
        assert_eq!(parse_docker_size("512kB"), 512_000);
        assert_eq!(parse_docker_size("512KB"), 512_000);
        assert_eq!(parse_docker_size("4.062TB"), 4_062_000_000_000);
    }

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_docker_size("1KiB"), 1024);
        assert_eq!(parse_docker_size("2GiB"), 2_147_483_648);
        assert_eq!(parse_docker_size("8.45GiB"), 9_073_118_413);
    }

    #[test]
    fn handles_zero_and_junk() {
        assert_eq!(parse_docker_size("0B"), 0);
        assert_eq!(parse_docker_size("0"), 0);
        assert_eq!(parse_docker_size(""), 0);
        assert_eq!(parse_docker_size("   "), 0);
        assert_eq!(parse_docker_size("not a size"), 0);
        // Negative would underflow the `as u64` cast into a huge number.
        assert_eq!(parse_docker_size("-5GB"), 0);
    }

    #[test]
    fn parses_bare_byte_counts() {
        assert_eq!(parse_docker_size("1024"), 1024);
    }

    #[test]
    fn reads_reclaimed_line() {
        let out = "deleted: sha256:abc\ndeleted: sha256:def\n\nTotal reclaimed space: 6.2GB";
        assert_eq!(parse_reclaimed(out), 6_200_000_000);
        assert_eq!(parse_reclaimed("Total reclaimed space: 0B"), 0);
        // buildctl prints a table with no such line.
        assert_eq!(parse_reclaimed("ID\tRECLAIMABLE\tSIZE\n"), 0);
        assert_eq!(parse_reclaimed(""), 0);
    }
}
