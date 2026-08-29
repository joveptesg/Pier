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

use crate::deploy::{
    apply_pier_networks, inject_mesh_extra_hosts_into_services, inject_ports_from_db,
    mesh_hosts_for_inject, project_network_for, strip_compose_ports,
};
use crate::docker::compose::{self, ComposeAuth};
use crate::state::AppState;

/// Inject mesh-DNS `extra_hosts:` into every `services:` block when
/// the WireGuard mesh is active. No-op otherwise, so non-mesh stacks
/// are byte-identical to what the operator wrote.
fn with_mesh_hosts(state: &AppState, yaml: &str) -> String {
    let hosts = mesh_hosts_for_inject(state);
    inject_mesh_extra_hosts_into_services(yaml, &hosts)
}

/// Put the stack on the network the service is assigned to, plus `pier-net`.
///
/// Same contract as the `.env` guarantee this module exists for: the network a
/// service runs on is decided by Pier, not by whatever the YAML happened to
/// say, and no deploy path gets to skip it. Applied on every deploy — create,
/// redeploy, restart, env change — so the invariant cannot drift.
fn with_pier_networks(state: &AppState, service_id: &str, yaml: &str) -> String {
    apply_pier_networks(yaml, &project_network_for(state, service_id))
}

/// Re-emit `ports:` from `port_allocations` instead of trusting the file.
///
/// The DB is the authority on which host port a service holds — it is what the
/// allocator handed out from the project range, what the public/private toggle
/// writes, and what a domain routes to. Re-deriving the section on every deploy
/// is what keeps that state from being reverted by the next `docker compose
/// up`. No-op when the service has no allocations.
fn with_pier_ports(state: &AppState, service_id: &str, yaml: &str) -> String {
    let has_rows = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT COUNT(*) FROM port_allocations WHERE service_id = ?1",
                [service_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        })
        .unwrap_or(0)
        > 0;
    if !has_rows {
        return yaml.to_string();
    }
    inject_ports_from_db(state, service_id, &strip_compose_ports(yaml))
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
    let yaml = with_pier_networks(state, service_id, yaml);
    let yaml = with_pier_ports(state, service_id, &yaml);
    let yaml = with_mesh_hosts(state, &yaml);
    compose::deploy_stack(stack_name, &yaml, &state.config, auth).await
}

/// Same as [`deploy_service_stack`], but streams compose output to `progress`
/// while it runs so the caller can surface image-pull progress, and fails
/// instead of hanging when the pull stalls.
pub async fn deploy_service_stack_with_progress(
    state: &AppState,
    service_id: &str,
    stack_name: &str,
    yaml: &str,
    auth: ComposeAuth,
    progress: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<String> {
    crate::deploy::write_env_file(state, service_id, stack_name).await;
    let yaml = with_pier_networks(state, service_id, yaml);
    let yaml = with_pier_ports(state, service_id, &yaml);
    let yaml = with_mesh_hosts(state, &yaml);
    compose::deploy_stack_with_progress(stack_name, &yaml, &state.config, auth, progress).await
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
    let yaml = with_pier_networks(state, service_id, yaml);
    let yaml = with_pier_ports(state, service_id, &yaml);
    let yaml = with_mesh_hosts(state, &yaml);
    compose::deploy_stack_no_cache(stack_name, &yaml, &state.config, auth).await
}
