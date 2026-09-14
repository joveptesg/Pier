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
    apply_pier_networks, inject_init_into_services, inject_mesh_extra_hosts_into_services,
    inject_ports_from_db, mesh_hosts_for_inject, project_network_for, strip_compose_ports,
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
    // Before stripping: a row left tagged NULL on a multi-service stack matches
    // no service, so inject would emit nothing for it, the container would come
    // up with no binding, and `port_sync` would reconcile the row to
    // `is_public = 0`. Resolution needs the `ports:` blocks strip is about to
    // remove, so it has to happen here rather than downstream.
    crate::deploy::backfill_orphan_ports(state, service_id, yaml);
    inject_ports_from_db(state, service_id, &strip_compose_ports(yaml))
}

/// Give every service an init process as PID 1, unless it declares `init:`
/// itself.
///
/// On by default, because PID 1 has no default signal handlers: an app without
/// its own SIGTERM handler ignores `docker stop` until the timeout turns it
/// into SIGKILL, and one that orphans children never reaps them. Measured on a
/// clean host, node and python went from a 31s SIGKILL to a sub-second clean
/// exit, and a node workload from 3495 zombies to none; postgres, redis, nginx
/// and traefik were unaffected either way.
///
/// Three ways to opt out, narrowest first: `init:` written in the operator's
/// own compose always wins, then the per-service toggle, then
/// `PIER_INJECT_INIT=0` for the whole host.
///
/// Skipped when the daemon reports no init binary: `init: true` against such a
/// host fails the whole `compose up`, which would turn a hardening measure into
/// an outage.
async fn with_pier_init(state: &AppState, service_id: &str, yaml: &str) -> String {
    // Host-wide kill switch first: `PIER_INJECT_INIT=0` silences this for every
    // service on the box, whatever the per-service toggles say.
    let host_disabled = std::env::var("PIER_INJECT_INIT")
        .map(|v| matches!(v.trim(), "0" | "false" | "no"))
        .unwrap_or(false);
    if host_disabled {
        return yaml.to_string();
    }
    // Then the service's own toggle, defaulting ON for rows that predate it.
    let enabled = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT inject_init FROM services WHERE id = ?1",
                [service_id],
                |row| row.get::<_, Option<bool>>(0),
            )
            .ok()
        })
        .flatten()
        .unwrap_or(true);
    if !enabled {
        return yaml.to_string();
    }
    let has_init_binary = state
        .docker
        .info()
        .await
        .ok()
        .and_then(|i| i.init_binary)
        .is_some_and(|b| !b.trim().is_empty());
    if !has_init_binary {
        tracing::warn!(
            "PIER_INJECT_INIT is set but the Docker daemon reports no init binary;              skipping injection rather than failing every deploy"
        );
        return yaml.to_string();
    }
    inject_init_into_services(yaml)
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
    let yaml = with_pier_init(state, service_id, &yaml).await;
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
    let yaml = with_pier_init(state, service_id, &yaml).await;
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
    let yaml = with_pier_init(state, service_id, &yaml).await;
    compose::deploy_stack_no_cache(stack_name, &yaml, &state.config, auth).await
}
