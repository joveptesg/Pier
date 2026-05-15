//! Service-bound compose deploy wrappers.
//!
//! Every code path that deploys a stack tied to a `services` row must go
//! through these wrappers. They guarantee that the encrypted `env_json` is
//! decrypted and materialized as `{stack_dir}/.env` *before* `docker compose
//! up` runs.
//!
//! Background: a previous post-mortem (commit 3398c29) documented seven
//! call sites that bypassed the crypto layer. By forcing callers to pass a
//! `service_id`, the contract is now expressed in the type signature — a new
//! deploy path cannot regress without explicitly omitting it.
//!
//! Service-less compose deploys (raw YAML in `api/compose.rs`, agent-proxied
//! deploys in `api/servers.rs`) keep using [`super::compose::deploy_stack`]
//! directly — they have no `env_json` to materialize.

use anyhow::Result;

use crate::deploy::{inject_mesh_extra_hosts_into_services, mesh_hosts_for_inject};
use crate::docker::compose::{self, ComposeAuth};
use crate::state::AppState;

/// Inject mesh-DNS `extra_hosts:` into every `services:` block when
/// the WireGuard mesh is active. No-op otherwise, so non-mesh stacks
/// are byte-identical to what the operator wrote.
fn with_mesh_hosts(state: &AppState, yaml: &str) -> String {
    let hosts = mesh_hosts_for_inject(state);
    inject_mesh_extra_hosts_into_services(yaml, &hosts)
}

/// Materialize `.env` from the service's encrypted `env_json` and run
/// `docker compose up -d`.
pub async fn deploy_service_stack(
    state: &AppState,
    service_id: &str,
    stack_name: &str,
    yaml: &str,
    auth: ComposeAuth,
) -> Result<String> {
    crate::deploy::write_env_file(state, service_id, stack_name).await;
    let yaml = with_mesh_hosts(state, yaml);
    compose::deploy_stack(stack_name, &yaml, &state.config, auth).await
}

/// Materialize `.env` from the service's encrypted `env_json` and run
/// `docker compose up -d --force-recreate --pull always` (no build cache).
pub async fn deploy_service_stack_no_cache(
    state: &AppState,
    service_id: &str,
    stack_name: &str,
    yaml: &str,
    auth: ComposeAuth,
) -> Result<String> {
    crate::deploy::write_env_file(state, service_id, stack_name).await;
    let yaml = with_mesh_hosts(state, yaml);
    compose::deploy_stack_no_cache(stack_name, &yaml, &state.config, auth).await
}
