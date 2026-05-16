//! Unified cron scheduler.
//!
//! One `tokio::interval(60s)` task scans the `schedules` table and fires
//! anything whose `next_run_at <= now()`. Actions are dispatched to
//! per-kind handlers in [`actions`].
//!
//! Action types currently supported:
//!
//! * **`action_type='task'`** — runs a saved task template on the chosen
//!   server. Inserts a [`crate::tasks`] run that the executor drives.
//! * **`action_type='backup'`** — fires a single backup via
//!   [`crate::backup::scheduler::run_for_schedule`].
//! * **`action_type='cleanup'`** — runs the Docker prune pipeline via
//!   [`crate::docker::cleanup::run_once`].

pub mod actions;
pub mod cron_utils;
pub mod runner;

pub use runner::start;
