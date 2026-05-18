//! Network-layer orchestration for the Pier mesh.
//!
//! * [`wireguard`] — IP allocation, `wg0.conf` rendering, and the
//!   typed view over the `wireguard_config` / `wireguard_peers` tables
//!   created in migration 41.
//! * [`mesh_call`] — uniform dispatch of helper ops to local or remote
//!   nodes, hiding whether the underlying transport is a unix socket
//!   (this host) or an HTTPS round-trip to `pier-agent` (remote host).

pub mod address;
pub mod mesh_call;
pub mod wireguard;
