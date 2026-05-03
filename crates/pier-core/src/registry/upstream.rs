//! Upstream proxy/cache for the embedded npm registry.
//!
//! When configured, missing packages are fetched from registry.npmjs.org
//! (or any compatible upstream), URL-rewritten so `dist.tarball` points at
//! us, and cached in `npm_packages`/`npm_versions` with `is_proxy = 1`.
//!
//! MVP: stub. Private publishing works without this. Wiring proxy mode is a
//! follow-up — see `registry-proxy.md` plan.

#![allow(dead_code)]

/// Default upstream for the proxy mode. Hard-coded for now; will move to a
/// settings row when proxy mode actually lands.
pub const DEFAULT_UPSTREAM: &str = "https://registry.npmjs.org";
