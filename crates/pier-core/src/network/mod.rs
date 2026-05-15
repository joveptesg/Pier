//! Network-layer orchestration for the Pier mesh.
//!
//! Currently houses the WireGuard mesh logic: IP allocation,
//! `wg0.conf` rendering, and the typed view over the `wireguard_config`
//! / `wireguard_peers` tables created in migration 41.

pub mod wireguard;
