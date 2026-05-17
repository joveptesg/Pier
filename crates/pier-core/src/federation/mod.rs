//! Read-only federation: keep a cached view of every peer's projects and
//! stacks so the dashboard can render an aggregated dashboard without
//! issuing N synchronous HTTP calls per page load.
//!
//! * [`sync`] runs as a background scheduler started in `main.rs` and
//!   refreshes `federated_projects` / `federated_stacks` for each
//!   `kind='peer'` server.
//! * [`client`] is the lightweight HTTP client primary-core uses to talk
//!   to each peer's `/api/v1/projects` and `/api/v1/stacks`.
//!
//! Write-federation (deploy/restart from primary UI) is intentionally
//! left out of this module — that's Etap 2 and gets its own surface.

pub mod client;
pub mod sync;
pub mod write_client;
