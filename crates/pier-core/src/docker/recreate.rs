//! Recreate containers with refreshed `HostConfig.PortBindings`.
//!
//! Powers the per-port public/private toggle for any service type
//! (catalog, git+Dockerfile, git+docker-compose, compose-template). The
//! function does NOT touch the docker-compose YAML on disk or
//! `services.compose_content` in the DB — configuration comes from the live
//! container via `inspect_container`. This makes the toggle independent of
//! whatever build strategy created the service in the first place, mirroring
//! the Coolify port-toggle model.
//!
//! Flow per container labeled `pier.service.id == <service_id>`:
//!   1. inspect       → grab ContainerConfig + HostConfig + network attachments
//!   2. compute new   → PortBindings + ExposedPorts from `port_allocations`
//!   3. pre-flight    → TcpListener::bind on each NEW public host port
//!   4. stop+remove   → tear down the running container
//!   5. create        → ContainerCreateBody copying everything from the old
//!      Config, swapping in the new HostConfig / ExposedPorts
//!   6. reattach      → user-defined networks the old container was on
//!   7. start         → bring it up; update `services.container_id`

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HostConfig, NetworkConnectRequest, PortBinding,
};
use bollard::query_parameters::{CreateContainerOptions, ListContainersOptions};

use crate::db::models::PortAllocation;
use crate::docker::containers;
use crate::state::AppState;

/// Recreate every container of `service_id` so its `HostConfig.PortBindings`
/// matches the current `port_allocations` rows. See module docs.
pub async fn recreate_with_port_bindings(state: &AppState, service_id: &str) -> Result<()> {
    let allocations = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        crate::db::ports::get_ports(&db, service_id)?
    };

    // Set of host ports this service legitimately claims via its
    // port_allocations rows. Pre-flight uses it as the first-line check: a
    // bind probe on one of "our" ports must never bail — the port is held
    // by either our current container (which stop+remove releases) or a
    // sibling we're recreating. pier::db::ports::allocate_ports refuses
    // already-used ports, so a row here means pier saw the port as free at
    // allocation time. A foreign process can't sneak in later without us
    // noticing on a prior recreate or sync.
    let our_host_ports: HashSet<u16> = allocations
        .iter()
        .map(|a| a.host_port as u16)
        .collect();

    let containers_list = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;

    // Primary lookup: by `pier.service.id` label (catalog-managed services).
    let mut target_ids: Vec<String> = containers_list
        .iter()
        .filter(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get("pier.service.id"))
                .is_some_and(|s| s == service_id)
        })
        .filter_map(|c| c.id.clone())
        .collect();

    // Fallback: git-deployed services and compose-template stacks built from a
    // user-authored docker-compose.yml don't carry `pier.service.id` on their
    // containers — Pier never gets a chance to inject the label, the operator
    // wrote the compose. Use `services.container_id` from the DB (or the
    // configured container_name) so the toggle works for those too. The new
    // container created below will get the label injected so future toggles
    // hit the fast path.
    if target_ids.is_empty() {
        let cid_or_name: Option<String> = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.query_row(
                "SELECT container_id FROM services WHERE id = ?1",
                [service_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        if let Some(cn) = cid_or_name {
            if !cn.is_empty() {
                if let Ok(info) = state.docker.inspect_container(&cn, None).await {
                    if let Some(id) = info.id {
                        tracing::info!(
                            "recreate_with_port_bindings: located container for {service_id} via services.container_id={cn}"
                        );
                        target_ids.push(id);
                    }
                } else {
                    tracing::warn!(
                        "recreate_with_port_bindings: services.container_id={cn} for {service_id} but Docker has no such container"
                    );
                }
            }
        }
    }

    if target_ids.is_empty() {
        // Truly no live container — caller may have toggled the DB flag
        // before the first deploy; the next deploy will pick up the new
        // allocations naturally.
        tracing::info!(
            "recreate_with_port_bindings: no container found for service_id={service_id} (no label, no DB container_id); skipping"
        );
        return Ok(());
    }

    // Multi-service docker-compose stack: if the container(s) we just found
    // carry a `com.docker.compose.project` label, pull in every sibling in
    // the same compose project so the service-level public-toggle recreates
    // *all* compose services together (api + max-bot for flowfin, not just
    // one of them). Without this, the fallback above only picks the single
    // container stored in `services.container_id` and silblings keep their
    // previous bindings — half the toggle silently no-ops. Catalog services
    // and standalone git-Dockerfile services don't get the compose project
    // label, so this is a no-op for them.
    {
        let project_label: Option<String> = containers_list
            .iter()
            .find(|c| c.id.as_deref() == Some(target_ids[0].as_str()))
            .and_then(|c| c.labels.as_ref())
            .and_then(|l| l.get("com.docker.compose.project"))
            .cloned();
        if let Some(project) = project_label {
            let existing: HashSet<String> = target_ids.iter().cloned().collect();
            let siblings: Vec<String> = containers_list
                .iter()
                .filter(|c| {
                    c.labels
                        .as_ref()
                        .and_then(|l| l.get("com.docker.compose.project"))
                        == Some(&project)
                })
                .filter(|c| {
                    c.labels
                        .as_ref()
                        .and_then(|l| l.get("com.docker.compose.service"))
                        .is_some()
                })
                .filter_map(|c| c.id.clone())
                .filter(|id| !existing.contains(id))
                .collect();
            if !siblings.is_empty() {
                tracing::info!(
                    "recreate_with_port_bindings: also found {} compose siblings for project {project}",
                    siblings.len()
                );
                target_ids.extend(siblings);
            }
        }
    }

    // `services.container_id` in Pier historically stores the **container
    // name** (e.g. `myhome-backend`), not the 64-char Docker ID — the UI's
    // "Internal Network" block displays this value as-is. Track the name we
    // assigned at create-time and write that back; fall back to the Docker
    // ID only when the container was created nameless (rare).
    let mut last_name_or_id: Option<String> = None;

    for cid in &target_ids {
        let cid = cid.as_str();
        let info = state.docker.inspect_container(cid, None).await?;

        let cfg = info
            .config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("inspect: container {cid} has no Config"))?
            .clone();
        let hc = info.host_config.clone().unwrap_or_default();
        let networks_old: HashMap<String, EndpointSettings> = info
            .network_settings
            .as_ref()
            .and_then(|ns| ns.networks.clone())
            .unwrap_or_default();

        let name = info
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default();

        let current_host_ports = collect_host_ports(&hc);
        let replica_idx_label = cfg
            .labels
            .as_ref()
            .and_then(|m| m.get("pier.replica.idx"))
            .map(|s| s.as_str());
        let compose_service_label = cfg
            .labels
            .as_ref()
            .and_then(|m| m.get("com.docker.compose.service"))
            .map(|s| s.as_str());

        let my_allocs: Vec<&PortAllocation> = allocations_for_this_container(
            &allocations,
            &current_host_ports,
            replica_idx_label,
            compose_service_label,
        );

        // Pre-flight: for every NEW public host port (one this container did
        // not already own), make sure no other host process is sitting on it.
        // Avoids deep Bollard errors and the "leftover Traefik / mosquitto"
        // foot-gun that bit srv1.
        //
        // Defensive: `current_host_ports` only sees what bollard returns in
        // `HostConfig.PortBindings`, which can be empty for compose-managed
        // containers in private mode even when docker-proxy is publishing on
        // 0.0.0.0 (the published binding comes from the YAML compose-up, not
        // from Pier). If the bind probe fails, fall back to scanning
        // `containers_list` — when the port owner is one of our own
        // `target_ids`, stop+remove below will free it, so skip. Only bail
        // when a *foreign* container or process owns the port.
        for a in &my_allocs {
            if !a.is_public {
                continue;
            }
            let Some(pp) = a.public_port else { continue };
            let pp = pp as u16;
            if current_host_ports.contains(&pp) {
                continue;
            }
            if our_host_ports.contains(&pp) {
                // Port is in our own port_allocations — whoever holds it on
                // the host is part of this service. stop+remove below will
                // free it, or it's already free because we own the slot.
                continue;
            }
            if let Err(e) = std::net::TcpListener::bind(("0.0.0.0", pp)) {
                let owner_id = find_port_owner_id(&containers_list, pp);
                let is_our_target = owner_id
                    .as_ref()
                    .is_some_and(|id| target_ids.iter().any(|t| t == id));
                if !is_our_target {
                    anyhow::bail!(
                        "Host port {pp} is already in use by another process (not this container): {e}. \
                         Free the port (e.g. `sudo ss -tlnp '( sport = :{pp} )'`) and toggle again."
                    );
                }
                tracing::info!(
                    "pre-flight: port {pp} held by our own target container {owner_id:?}; will release on stop+remove"
                );
            }
        }

        let new_bindings = build_port_bindings_for_container(&my_allocs);
        let new_exposed = build_exposed_ports(&cfg.exposed_ports, &my_allocs);

        // Inject `pier.service.id` (and the optional `pier.managed` marker)
        // so the next toggle's primary label lookup hits this container
        // directly — without needing the DB fallback. Git-deployed services
        // that started out unlabelled get adopted after their first toggle.
        let mut new_labels = cfg.labels.clone().unwrap_or_default();
        new_labels.insert("pier.service.id".to_string(), service_id.to_string());
        new_labels
            .entry("pier.managed".to_string())
            .or_insert_with(|| "true".to_string());

        let new_host_cfg = HostConfig {
            port_bindings: Some(new_bindings),
            binds: hc.binds.clone(),
            restart_policy: hc.restart_policy.clone(),
            network_mode: hc.network_mode.clone(),
            extra_hosts: hc.extra_hosts.clone(),
            dns: hc.dns.clone(),
            dns_search: hc.dns_search.clone(),
            dns_options: hc.dns_options.clone(),
            devices: hc.devices.clone(),
            cap_add: hc.cap_add.clone(),
            cap_drop: hc.cap_drop.clone(),
            security_opt: hc.security_opt.clone(),
            ulimits: hc.ulimits.clone(),
            mounts: hc.mounts.clone(),
            log_config: hc.log_config.clone(),
            sysctls: hc.sysctls.clone(),
            tmpfs: hc.tmpfs.clone(),
            group_add: hc.group_add.clone(),
            ipc_mode: hc.ipc_mode.clone(),
            pid_mode: hc.pid_mode.clone(),
            uts_mode: hc.uts_mode.clone(),
            userns_mode: hc.userns_mode.clone(),
            shm_size: hc.shm_size,
            privileged: hc.privileged,
            readonly_rootfs: hc.readonly_rootfs,
            auto_remove: hc.auto_remove,
            init: hc.init,
            oom_score_adj: hc.oom_score_adj,
            ..Default::default()
        };

        let new_body = ContainerCreateBody {
            image: cfg.image.clone(),
            hostname: cfg.hostname.clone(),
            domainname: cfg.domainname.clone(),
            user: cfg.user.clone(),
            env: cfg.env.clone(),
            cmd: cfg.cmd.clone(),
            entrypoint: cfg.entrypoint.clone(),
            working_dir: cfg.working_dir.clone(),
            labels: Some(new_labels),
            healthcheck: cfg.healthcheck.clone(),
            volumes: cfg.volumes.clone(),
            stop_signal: cfg.stop_signal.clone(),
            stop_timeout: cfg.stop_timeout,
            shell: cfg.shell.clone(),
            tty: cfg.tty,
            open_stdin: cfg.open_stdin,
            stdin_once: cfg.stdin_once,
            attach_stdin: cfg.attach_stdin,
            attach_stdout: cfg.attach_stdout,
            attach_stderr: cfg.attach_stderr,
            exposed_ports: Some(new_exposed),
            host_config: Some(new_host_cfg),
            ..Default::default()
        };

        tracing::info!(
            "recreate_with_port_bindings: recreating {name} ({cid}) with {} port bindings",
            new_body
                .host_config
                .as_ref()
                .and_then(|h| h.port_bindings.as_ref())
                .map(|m| m.len())
                .unwrap_or(0)
        );

        containers::stop_container(&state.docker, cid).await?;
        containers::remove_container(&state.docker, cid, true).await?;

        let created = state
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: if name.is_empty() {
                        None
                    } else {
                        Some(name.clone())
                    },
                    ..Default::default()
                }),
                new_body,
            )
            .await?;

        // Reattach any user-defined network the old container was on but the
        // new one isn't (the new one is on whichever single network the body's
        // network_mode pointed at, if any).
        let new_id = created.id.clone();
        for net_name in networks_old.keys() {
            let req = NetworkConnectRequest {
                container: new_id.clone(),
                endpoint_config: Some(EndpointSettings::default()),
            };
            match state.docker.connect_network(net_name, req).await {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if !msg.contains("already exists in network")
                        && !msg.contains("already attached to network")
                    {
                        tracing::warn!(
                            "recreate: failed to re-attach {service_id} to network {net_name}: {msg}"
                        );
                    }
                }
            }
        }

        containers::start_container(&state.docker, &new_id).await?;
        // Prefer the container name we just (re)used at create-time — Pier's
        // historical convention for services.container_id. Empty name means
        // the create_container call passed None (very rare); fall back to
        // the Docker ID so the row isn't left stale.
        last_name_or_id = Some(if name.is_empty() {
            new_id.clone()
        } else {
            name.clone()
        });
    }

    // Update services.container_id (= container name) so the UI's
    // "Internal Network" block keeps showing `<name>:<port>` instead of a
    // 64-char SHA256. For multi-replica services this is "any one of
    // them"; production read paths that need to be exact should use the
    // `pier.service.id` label anyway.
    if let Some(name_or_id) = last_name_or_id {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE services SET container_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![name_or_id, service_id],
        );
    }

    Ok(())
}

/// Extract every host port number this container is currently publishing.
/// Used to (a) skip pre-flight bind probes for ports we already own, (b) pick
/// the right `port_allocations` rows for a multi-replica container.
fn collect_host_ports(hc: &HostConfig) -> HashSet<u16> {
    let mut out = HashSet::new();
    let Some(bindings) = hc.port_bindings.as_ref() else {
        return out;
    };
    for entries in bindings.values().flatten() {
        for entry in entries {
            if let Some(hp) = entry.host_port.as_deref() {
                if let Ok(p) = hp.parse::<u16>() {
                    out.insert(p);
                }
            }
        }
    }
    out
}

/// Pick the subset of `port_allocations` rows that belong to this specific
/// container instance.
///
/// Multi-service docker-compose stacks (flowfin: `api` + `max-bot`): each
/// container carries the Compose-injected label
/// `com.docker.compose.service` whose value matches one
/// `port_allocations.compose_service`. When that label is present we
/// partition by it — otherwise the recreate of `flowfin-api` would inherit
/// max-bot's `0.0.0.0:3054` binding and collide with the still-live sibling.
/// This branch takes precedence over the replica logic below; multi-service
/// stacks never share host ports between siblings by construction.
///
/// Single-replica multi-port services (myhome-backend: `primary` + `port-1`):
/// every container is "the only one", returns all rows.
///
/// Multi-replica services (Postgres cluster, etc.): each container has a
/// unique `host_port`. Match rows by host_port if the container currently
/// publishes anything, otherwise fall back to the `pier.replica.idx` label
/// combined with `port_name` like `replica_<idx>`.
fn allocations_for_this_container<'a>(
    allocations: &'a [PortAllocation],
    current_host_ports: &HashSet<u16>,
    replica_idx_label: Option<&str>,
    container_compose_service: Option<&str>,
) -> Vec<&'a PortAllocation> {
    if let Some(cs) = container_compose_service {
        // Catalog single-service guard: `deploy::update_ports_from_compose`
        // stores `compose_service = NULL` when the generated YAML has only
        // one service, even though the live container carries Docker's
        // `com.docker.compose.service` label. Without this branch the strict
        // partition below filters out every row (`None != Some("postgresql")`)
        // and the toggle's recreate produces 0 port bindings → docker-proxy
        // never starts → UI toggle bounces back. Same degraded-match logic
        // that `port_sync::compute_sync_updates` already uses.
        let all_alloc_null = allocations.iter().all(|a| a.compose_service.is_none());
        if all_alloc_null {
            return allocations.iter().collect();
        }
        return allocations
            .iter()
            .filter(|a| a.compose_service.as_deref() == Some(cs))
            .collect();
    }

    let is_multi_replica = allocations
        .iter()
        .any(|a| a.port_name.starts_with("replica_") || a.port_name.starts_with("replica-"));

    if !is_multi_replica {
        return allocations.iter().collect();
    }

    if !current_host_ports.is_empty() {
        return allocations
            .iter()
            .filter(|a| current_host_ports.contains(&(a.host_port as u16)))
            .collect();
    }

    if let Some(idx) = replica_idx_label {
        let needles = [format!("replica_{idx}"), format!("replica-{idx}")];
        return allocations
            .iter()
            .filter(|a| needles.iter().any(|n| n == &a.port_name))
            .collect();
    }

    Vec::new()
}

/// Find which container in `containers_list` is publishing host port `pp`
/// (i.e. which container's `Ports` entry has `public_port == Some(pp)`).
/// Returns its id when known. Used by pre-flight to distinguish a port held
/// by our own to-be-recreated container from a port held by an unrelated
/// process — the former we can release by stop+remove, the latter is a real
/// foot-gun and we should bail.
fn find_port_owner_id(
    containers_list: &[bollard::models::ContainerSummary],
    pp: u16,
) -> Option<String> {
    containers_list
        .iter()
        .find(|c| {
            c.ports
                .as_ref()
                .is_some_and(|ps| ps.iter().any(|p| p.public_port == Some(pp)))
        })
        .and_then(|c| c.id.clone())
}

/// Build `HostConfig.PortBindings` for one container instance.
///
/// Only `is_public=1` rows produce a host binding (`0.0.0.0:public_port:container_port`).
/// Private rows get nothing here — the container stays reachable through
/// `pier-net` by service name without any host port published. This mirrors
/// what `inject_ports_from_db` writes into the compose file and what Coolify's
/// "Ports Mappings" feature does: a port is either explicitly published or
/// it isn't, no hidden localhost binding in between.
///
/// `container_port:0` or missing `public_port` on a public row → skip
/// (corrupt state; let the recreate continue rather than blowing up).
pub(crate) fn build_port_bindings_for_container(
    allocations: &[&PortAllocation],
) -> HashMap<String, Option<Vec<PortBinding>>> {
    let mut out: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for a in allocations {
        if a.container_port <= 0 || !a.is_public {
            continue;
        }
        let Some(pp) = a.public_port else { continue };
        let proto = if a.protocol.is_empty() {
            "tcp"
        } else {
            a.protocol.as_str()
        };
        let key = format!("{}/{}", a.container_port, proto);

        let binding = PortBinding {
            host_ip: Some("0.0.0.0".to_string()),
            host_port: Some(pp.to_string()),
        };
        out.entry(key.clone()).or_insert_with(|| Some(Vec::new()));
        if let Some(list) = out.get_mut(&key) {
            if let Some(v) = list.as_mut() {
                v.push(binding);
            }
        }
    }
    out
}

/// Union of the old container's `ExposedPorts` and the container ports we're
/// about to bind. Docker rejects a `PortBinding` for a port not in
/// `ExposedPorts`, so we make sure every new binding has a matching entry.
fn build_exposed_ports(
    old_exposed: &Option<Vec<String>>,
    allocations: &[&PortAllocation],
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    if let Some(v) = old_exposed {
        for s in v {
            if seen.insert(s.clone()) {
                out.push(s.clone());
            }
        }
    }
    for a in allocations {
        if a.container_port <= 0 {
            continue;
        }
        let proto = if a.protocol.is_empty() {
            "tcp"
        } else {
            a.protocol.as_str()
        };
        let key = format!("{}/{}", a.container_port, proto);
        if seen.insert(key.clone()) {
            out.push(key);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc(
        name: &str,
        host: i64,
        container: i64,
        is_public: bool,
        public_port: Option<i64>,
    ) -> PortAllocation {
        PortAllocation {
            id: format!("id-{name}-{container}"),
            service_id: "svc".to_string(),
            port_name: name.to_string(),
            host_port: host,
            container_port: container,
            protocol: "tcp".to_string(),
            is_public,
            public_port,
            created_at: String::new(),
            compose_service: None,
        }
    }

    #[test]
    fn single_replica_single_public_port() {
        let a = alloc("primary", 4471, 4471, true, Some(4471));
        let refs: Vec<&PortAllocation> = vec![&a];
        let b = build_port_bindings_for_container(&refs);
        let entry = b
            .get("4471/tcp")
            .expect("4471/tcp present")
            .as_ref()
            .unwrap();
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(entry[0].host_port.as_deref(), Some("4471"));
    }

    #[test]
    fn single_replica_single_private_port_emits_no_binding() {
        // Private ports get no host binding at all — they stay reachable
        // through pier-net by service name, no `-p` published. This matches
        // what inject_ports_from_db writes into compose.
        let a = alloc("primary", 10042, 8080, false, None);
        let refs: Vec<&PortAllocation> = vec![&a];
        let b = build_port_bindings_for_container(&refs);
        assert!(
            b.is_empty(),
            "private port should produce no bindings: {b:?}"
        );
    }

    #[test]
    fn myhome_backend_two_public_ports() {
        let a = alloc("primary", 4471, 4471, true, Some(4471));
        let b = alloc("port-1", 1883, 1883, true, Some(1883));
        let refs: Vec<&PortAllocation> = vec![&a, &b];
        let m = build_port_bindings_for_container(&refs);
        assert!(m.contains_key("4471/tcp"));
        assert!(m.contains_key("1883/tcp"));
        let e1 = m.get("4471/tcp").unwrap().as_ref().unwrap();
        let e2 = m.get("1883/tcp").unwrap().as_ref().unwrap();
        assert_eq!(e1[0].host_port.as_deref(), Some("4471"));
        assert_eq!(e1[0].host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(e2[0].host_port.as_deref(), Some("1883"));
        assert_eq!(e2[0].host_ip.as_deref(), Some("0.0.0.0"));
    }

    #[test]
    fn one_public_one_private_emits_only_public() {
        // Public row → 0.0.0.0 binding. Private row → no binding (the
        // 1883/tcp key shouldn't appear in the map at all).
        let pub_p = alloc("primary", 4471, 4471, true, Some(4471));
        let priv_p = alloc("port-1", 1883, 1883, false, None);
        let refs: Vec<&PortAllocation> = vec![&pub_p, &priv_p];
        let m = build_port_bindings_for_container(&refs);
        assert_eq!(
            m.get("4471/tcp").unwrap().as_ref().unwrap()[0]
                .host_ip
                .as_deref(),
            Some("0.0.0.0")
        );
        assert!(
            !m.contains_key("1883/tcp"),
            "private port leaked into bindings: {m:?}"
        );
    }

    #[test]
    fn empty_allocations_yields_empty_map() {
        let refs: Vec<&PortAllocation> = vec![];
        let m = build_port_bindings_for_container(&refs);
        assert!(m.is_empty());
    }

    #[test]
    fn skips_public_row_without_public_port() {
        // Corrupt state — is_public=1 but public_port is NULL. Skip rather
        // than panicking on unwrap.
        let a = alloc("primary", 4471, 4471, true, None);
        let refs: Vec<&PortAllocation> = vec![&a];
        let m = build_port_bindings_for_container(&refs);
        assert!(m.is_empty());
    }

    #[test]
    fn allocations_filter_single_replica_returns_all() {
        let a = alloc("primary", 4471, 4471, true, Some(4471));
        let b = alloc("port-1", 1883, 1883, true, Some(1883));
        let allocs = vec![a, b];
        let mine = allocations_for_this_container(&allocs, &HashSet::new(), None, None);
        assert_eq!(mine.len(), 2);
    }

    #[test]
    fn allocations_filter_multi_replica_by_host_port() {
        let a = alloc("replica_1", 7000, 5432, false, None);
        let b = alloc("replica_2", 7001, 5432, false, None);
        let c = alloc("replica_3", 7002, 5432, false, None);
        let allocs = vec![a, b, c];
        let mut mine_hp = HashSet::new();
        mine_hp.insert(7001u16);
        let mine = allocations_for_this_container(&allocs, &mine_hp, None, None);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].port_name, "replica_2");
    }

    #[test]
    fn allocations_filter_multi_replica_by_label_when_no_bindings() {
        let a = alloc("replica_1", 7000, 5432, false, None);
        let b = alloc("replica_2", 7001, 5432, false, None);
        let allocs = vec![a, b];
        // current_host_ports empty (container had no publish), fall back to label.
        let mine = allocations_for_this_container(&allocs, &HashSet::new(), Some("2"), None);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].port_name, "replica_2");
    }

    fn alloc_with_compose(
        name: &str,
        host: i64,
        container: i64,
        is_public: bool,
        public_port: Option<i64>,
        compose_service: &str,
    ) -> PortAllocation {
        let mut a = alloc(name, host, container, is_public, public_port);
        a.compose_service = Some(compose_service.to_string());
        a
    }

    #[test]
    fn allocations_filter_multi_service_compose_partitions_by_compose_service() {
        // flowfin: two compose services (api, max-bot), each with one port
        // named "primary". Recreating `flowfin-api` must NOT pull in
        // max-bot's 3054 — that's the bug that caused the
        // "port is already allocated" 500.
        let api = alloc_with_compose("primary", 3050, 3050, false, None, "api");
        let bot = alloc_with_compose("primary", 3054, 3054, false, None, "max-bot");
        let allocs = vec![api, bot];

        let mut current = HashSet::new();
        current.insert(3050u16);
        let mine =
            allocations_for_this_container(&allocs, &current, None, Some("api"));
        assert_eq!(mine.len(), 1, "api container must get only its own row, got {mine:?}");
        assert_eq!(mine[0].host_port, 3050);
        assert_eq!(mine[0].compose_service.as_deref(), Some("api"));
    }

    #[test]
    fn allocations_filter_single_service_catalog_with_compose_label() {
        // Regression: postgres (single-service catalog) stores
        // compose_service=NULL in port_allocations, but the live container
        // carries docker-compose's auto-injected label
        // com.docker.compose.service="postgresql". Strict partition would
        // return empty (None != Some("postgresql")), the toggle would
        // recreate the container with 0 bindings, docker-proxy would never
        // start, and the UI toggle would bounce back to OFF.
        let a = alloc("primary", 10000, 5432, true, Some(5432));
        assert!(a.compose_service.is_none(), "precondition: catalog row stores NULL");
        let allocs = vec![a];
        let mine = allocations_for_this_container(
            &allocs,
            &HashSet::new(),
            None,
            Some("postgresql"),
        );
        assert_eq!(
            mine.len(),
            1,
            "single-service catalog must not partition out its only row"
        );
        assert_eq!(mine[0].host_port, 10000);
    }

    #[test]
    fn allocations_filter_legacy_null_compose_service_returns_all() {
        // Single-container catalog services (or pre-b84aa79 deployments) have
        // compose_service = NULL on every row, and the container has no
        // com.docker.compose.service label. Behavior must match the old code:
        // return everything.
        let a = alloc("primary", 4471, 4471, true, Some(4471));
        let b = alloc("port-1", 1883, 1883, true, Some(1883));
        let allocs = vec![a, b];
        let mine = allocations_for_this_container(&allocs, &HashSet::new(), None, None);
        assert_eq!(mine.len(), 2);
    }

    #[test]
    fn allocations_filter_compose_branch_takes_precedence_over_replica() {
        // Defensive: even if the rows happen to look replica-like, the
        // compose-service label (Docker's source of truth for which container
        // we're touching) wins. Prevents the partition logic from falling
        // through to the `replica_*` branch and matching by host_port.
        let a = alloc_with_compose("replica_1", 7000, 5432, false, None, "db-primary");
        let b = alloc_with_compose("replica_2", 7001, 5432, false, None, "db-replica");
        let allocs = vec![a, b];
        let mine =
            allocations_for_this_container(&allocs, &HashSet::new(), Some("1"), Some("db-replica"));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].compose_service.as_deref(), Some("db-replica"));
    }

    fn container_summary(id: &str, host_published_ports: &[u16]) -> bollard::models::ContainerSummary {
        let ports: Vec<bollard::models::PortSummary> = host_published_ports
            .iter()
            .map(|&hp| bollard::models::PortSummary {
                ip: Some("0.0.0.0".to_string()),
                private_port: hp,
                public_port: Some(hp),
                typ: None,
            })
            .collect();
        bollard::models::ContainerSummary {
            id: Some(id.to_string()),
            ports: Some(ports),
            ..Default::default()
        }
    }

    #[test]
    fn find_port_owner_returns_id_when_container_publishes_port() {
        let list = vec![
            container_summary("c-api", &[3050]),
            container_summary("c-bot", &[3054]),
        ];
        assert_eq!(find_port_owner_id(&list, 3050), Some("c-api".to_string()));
        assert_eq!(find_port_owner_id(&list, 3054), Some("c-bot".to_string()));
    }

    #[test]
    fn find_port_owner_returns_none_when_no_container_publishes_port() {
        let list = vec![container_summary("c-api", &[3050])];
        assert_eq!(find_port_owner_id(&list, 9999), None);
    }

    #[test]
    fn preflight_is_our_target_when_owner_is_in_target_ids() {
        // Regression for the flowfin "port already allocated" loop: the
        // port is held by our own to-be-recreated container — pre-flight
        // must not bail, stop+remove below will release it.
        let list = vec![container_summary("c-api", &[3050])];
        let target_ids = ["c-api".to_string(), "c-bot".to_string()];
        let owner_id = find_port_owner_id(&list, 3050);
        let is_our_target = owner_id
            .as_ref()
            .is_some_and(|id| target_ids.iter().any(|t| t == id));
        assert!(is_our_target);
    }

    #[test]
    fn preflight_is_not_our_target_when_owner_is_stranger() {
        // Leftover Traefik / mosquitto foot-gun: a container holds the
        // port but is not in our target set — pre-flight must bail.
        let list = vec![container_summary("c-traefik-zombie", &[3050])];
        let target_ids = ["c-api".to_string()];
        let owner_id = find_port_owner_id(&list, 3050);
        let is_our_target = owner_id
            .as_ref()
            .is_some_and(|id| target_ids.iter().any(|t| t == id));
        assert!(!is_our_target);
    }

    #[test]
    fn our_host_ports_collected_from_allocations() {
        // The set used by pre-flight to skip self-owned ports. Built from
        // ALL allocations of the service (not just my_allocs of the current
        // container) so any sibling-held port also skips.
        let api = alloc_with_compose("primary", 3050, 3050, true, Some(3050), "api");
        let bot = alloc_with_compose("primary", 3054, 3054, false, None, "max-bot");
        let allocs = [api, bot];
        let our: std::collections::HashSet<u16> =
            allocs.iter().map(|a| a.host_port as u16).collect();
        assert!(our.contains(&3050), "api's 3050 must be in our_host_ports");
        assert!(our.contains(&3054), "max-bot's 3054 must be in our_host_ports");
        assert!(!our.contains(&9999), "unrelated port must NOT be in set");
    }

    #[test]
    fn preflight_is_not_our_target_when_owner_unknown() {
        // Port held by a non-Docker process (random host service). Bail.
        let list: Vec<bollard::models::ContainerSummary> = vec![];
        let target_ids = ["c-api".to_string()];
        let owner_id = find_port_owner_id(&list, 3050);
        let is_our_target = owner_id
            .as_ref()
            .is_some_and(|id| target_ids.iter().any(|t| t == id));
        assert!(!is_our_target);
    }

    #[test]
    fn build_port_bindings_for_container_with_partitioned_allocations_yields_single_binding() {
        // End-to-end of the fix: after compose-service partitioning, the
        // bindings HashMap fed into Docker create has exactly one entry per
        // sibling — no chance of accidentally republishing a neighbour's port.
        let api = alloc_with_compose("primary", 3050, 3050, true, Some(3050), "api");
        let bot = alloc_with_compose("primary", 3054, 3054, true, Some(3054), "max-bot");
        let allocs = vec![api, bot];
        let mine =
            allocations_for_this_container(&allocs, &HashSet::new(), None, Some("api"));
        let b = build_port_bindings_for_container(&mine);
        assert_eq!(b.len(), 1, "expected one binding for api container, got {b:?}");
        assert!(b.contains_key("3050/tcp"));
        assert!(!b.contains_key("3054/tcp"), "max-bot's port leaked: {b:?}");
    }

    #[test]
    fn exposed_ports_union_old_and_new() {
        let old = Some(vec!["80/tcp".to_string(), "443/tcp".to_string()]);
        let a = alloc("primary", 4471, 4471, true, Some(4471));
        let refs: Vec<&PortAllocation> = vec![&a];
        let out = build_exposed_ports(&old, &refs);
        assert!(out.contains(&"80/tcp".to_string()));
        assert!(out.contains(&"443/tcp".to_string()));
        assert!(out.contains(&"4471/tcp".to_string()));
    }
}
