//! Process-global host CPU sampler.
//!
//! `sysinfo` computes CPU usage as the delta between two successive samples of
//! `/proc/stat` taken at least [`sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`] apart.
//! Reading `global_cpu_usage()` from a freshly-created `System` (a single
//! sample) therefore always returns `0.0`. To avoid that, we keep one
//! long-lived `System` in a background task that refreshes on a fixed cadence,
//! and cache the latest value here so any handler can read a real, non-zero
//! figure without owning its own sampler.
//!
//! CPU load is host-global, so a single process-wide cache is the natural fit —
//! it also serves the alert path (`alerts::metrics::fetch_local_host_metric`)
//! which has no access to `AppState`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sysinfo::System;

/// Latest global CPU usage percentage, stored as the bit pattern of an `f32`.
static CPU_USAGE: AtomicU32 = AtomicU32::new(0);

/// How often the background task re-samples CPU usage.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Current host CPU usage as a percentage in `0.0..=100.0`.
///
/// Returns `0.0` until the sampler has taken its second sample (roughly
/// [`sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`] after [`spawn_sampler`] runs).
pub fn current() -> f32 {
    f32::from_bits(CPU_USAGE.load(Ordering::Relaxed))
}

/// Spawn the background CPU sampler. Call exactly once at startup.
///
/// Keeps a single `System` alive and refreshes CPU usage on a fixed cadence, so
/// every refresh after the first produces a real delta-based reading.
pub fn spawn_sampler() {
    tokio::spawn(async move {
        // First sample. `new_all()` populates the CPU list and takes the
        // initial reading; the very next refresh yields a usable delta.
        let mut sys = System::new_all();
        sys.refresh_cpu_usage();
        tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;

        loop {
            sys.refresh_cpu_usage();
            CPU_USAGE.store(sys.global_cpu_usage().to_bits(), Ordering::Relaxed);
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
}
