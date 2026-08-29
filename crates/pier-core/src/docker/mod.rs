pub mod auth;
pub mod cleanup;
pub mod compose;
pub mod compose_service;
pub mod containers;
pub mod deploy_reconcile;
pub mod events;
pub mod image_gc;
pub mod images;
pub mod logs;
pub mod pgdata_migration;
pub mod port_sync;
pub mod recreate;

pub use compose_service::{
    deploy_service_stack, deploy_service_stack_no_cache, deploy_service_stack_with_progress,
};

/// A `docker` CLI command pre-pointed at the daemon Pier actually manages.
///
/// bollard honours `PIER_DOCKER_HOST`, but a bare `Command::new("docker")`
/// inherits the daemon from the process environment instead. On a host
/// configured against a remote daemon that split meant every prune ran on the
/// local one — cleaning a machine nothing was deployed to while the real host
/// kept filling up.
pub fn docker_cmd(state: &crate::state::SharedState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("docker");
    if let Some(host) = &state.config.docker_host {
        cmd.env("DOCKER_HOST", host);
    }
    cmd
}
