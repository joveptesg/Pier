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

    let mut last_new_id: Option<String> = None;

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

        let my_allocs: Vec<&PortAllocation> =
            allocations_for_this_container(&allocations, &current_host_ports, replica_idx_label);

        // Pre-flight: for every NEW public host port (one this container did
        // not already own), make sure no other host process is sitting on it.
        // Avoids deep Bollard errors and the "leftover Traefik / mosquitto"
        // foot-gun that bit srv1.
        for a in &my_allocs {
            if !a.is_public {
                continue;
            }
            let Some(pp) = a.public_port else { continue };
            let pp = pp as u16;
            if current_host_ports.contains(&pp) {
                continue;
            }
            if let Err(e) = std::net::TcpListener::bind(("0.0.0.0", pp)) {
                anyhow::bail!(
                    "Host port {pp} is already in use by another process (not this container): {e}. \
                     Free the port (e.g. `sudo ss -tlnp '( sport = :{pp} )'`) and toggle again."
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
        last_new_id = Some(new_id);
    }

    // Update services.container_id to the most recently created id so other
    // single-container read paths keep working. For multi-replica services
    // this is "any one of them"; production read paths that care should
    // already be label-based, not id-based.
    if let Some(new_id) = last_new_id {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "UPDATE services SET container_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![new_id, service_id],
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
) -> Vec<&'a PortAllocation> {
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

/// Build `HostConfig.PortBindings` for one container instance.
///
/// Convention (mirrors what `build_compose_yaml_scaled` used to emit so the
/// behavior of catalog services is byte-for-byte identical after recreate):
///   - `is_public=1` → `0.0.0.0:public_port:container_port` (raw public TCP)
///   - `is_public=0` → `127.0.0.1:host_port:container_port` (localhost-only,
///     still reachable from the host for ops/debug)
///
/// `container_port:0` or missing `public_port` rows are skipped (corrupt
/// state — let the deploy carry on rather than blowing up the recreate).
pub(crate) fn build_port_bindings_for_container(
    allocations: &[&PortAllocation],
) -> HashMap<String, Option<Vec<PortBinding>>> {
    let mut out: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
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

        let binding = if a.is_public {
            let Some(pp) = a.public_port else { continue };
            PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(pp.to_string()),
            }
        } else {
            PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(a.host_port.to_string()),
            }
        };
        out.entry(key).or_insert_with(|| Some(Vec::new()));
        if let Some(list) = out.get_mut(&format!("{}/{}", a.container_port, proto)) {
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
    fn single_replica_single_private_port_binds_localhost() {
        let a = alloc("primary", 10042, 8080, false, None);
        let refs: Vec<&PortAllocation> = vec![&a];
        let b = build_port_bindings_for_container(&refs);
        let entry = b
            .get("8080/tcp")
            .expect("8080/tcp present")
            .as_ref()
            .unwrap();
        assert_eq!(entry.len(), 1);
        assert_eq!(entry[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(entry[0].host_port.as_deref(), Some("10042"));
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
    fn one_public_one_private_separate_addresses() {
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
        assert_eq!(
            m.get("1883/tcp").unwrap().as_ref().unwrap()[0]
                .host_ip
                .as_deref(),
            Some("127.0.0.1")
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
        let mine = allocations_for_this_container(&allocs, &HashSet::new(), None);
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
        let mine = allocations_for_this_container(&allocs, &mine_hp, None);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].port_name, "replica_2");
    }

    #[test]
    fn allocations_filter_multi_replica_by_label_when_no_bindings() {
        let a = alloc("replica_1", 7000, 5432, false, None);
        let b = alloc("replica_2", 7001, 5432, false, None);
        let allocs = vec![a, b];
        // current_host_ports empty (container had no publish), fall back to label.
        let mine = allocations_for_this_container(&allocs, &HashSet::new(), Some("2"));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].port_name, "replica_2");
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
