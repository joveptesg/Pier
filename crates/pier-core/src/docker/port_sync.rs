//! Synchronize Pier's `port_allocations` table with what Docker actually
//! publishes for a service's containers.
//!
//! Pier's `parse_compose_services` deliberately ignores the host-IP portion
//! of `ports:` entries (so `"0.0.0.0:3050:3050"` and `"127.0.0.1:3050:3050"`
//! both become `(3050, 3050)`) and `update_ports_from_compose` never touches
//! `is_public` — that flag is owned by the UI toggle path. The consequence:
//! if an operator-authored `docker-compose.yml` publishes ports on `0.0.0.0`,
//! `docker compose up` honours it, the container becomes public on the host,
//! but the DB row stays `is_public=0`. The UI then renders the toggle as
//! OFF over a container that is actually public, and the next toggle press
//! fails at recreate-time on its own `docker-proxy`.
//!
//! This module reads each container's `HostConfig.PortBindings` (the source
//! of truth — what Docker actually wired up), and updates the DB rows so the
//! UI and the toggle path see reality. Coolify-style.
//!
//! Failures are best-effort: page-load sync never blocks the UI. If Docker
//! is unreachable the existing DB rows are used as-is.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use bollard::query_parameters::ListContainersOptions;

use crate::db::models::PortAllocation;
use crate::state::{AppState, SharedState};

/// Minimum gap between two background port syncs for the same service. The
/// resource-detail page polls every 5s and an operator may have several tabs
/// open; without this throttle each tick would re-hammer the Docker API. A
/// service that synced (or is mid-sync) inside this window is skipped.
const SYNC_TTL: Duration = Duration::from_secs(8);

/// Per-service timestamp of the last background sync kickoff. Module-local so
/// we don't have to thread new state through `AppState` and its constructor.
fn last_sync_map() -> &'static Mutex<HashMap<String, Instant>> {
    static M: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fire-and-forget, throttled background port reconciliation. Kept OFF the
/// read path so `GET /api/v1/resources/{id}` returns immediately instead of
/// blocking on `docker inspect`. Best-effort: any DB correction is surfaced
/// to the UI by its existing 5s poll. The make-public toggle path
/// (`set_port_public`) still syncs synchronously, where correctness matters.
pub fn spawn_port_sync_throttled(state: SharedState, service_id: String) {
    {
        let mut m = match last_sync_map().lock() {
            Ok(g) => g,
            Err(_) => return, // poisoned — skip rather than panic on a best-effort path
        };
        if let Some(t) = m.get(&service_id) {
            if t.elapsed() < SYNC_TTL {
                return; // synced recently (or a sync is still in flight)
            }
        }
        // Stamp BEFORE spawning so concurrent tabs dedupe against this entry.
        m.insert(service_id.clone(), Instant::now());
    }
    tokio::spawn(async move {
        if let Err(e) = sync_ports_from_docker(&state, &service_id).await {
            tracing::warn!("background port sync for {service_id} failed: {e}");
        }
    });
}

/// What Docker reports for one container — the compose service name (from
/// the `com.docker.compose.service` label) and the host bindings extracted
/// from `HostConfig.PortBindings`.
pub(crate) struct ContainerBindings {
    pub compose_service: Option<String>,
    pub bindings: Vec<ContainerBinding>,
}

pub(crate) struct ContainerBinding {
    pub container_port: u16,
    pub host_port: u16,
    /// `true` when Docker reports an empty / `0.0.0.0` / `::` host-IP —
    /// i.e. the binding is reachable from outside the host. `false` when
    /// Docker is publishing on `127.0.0.1` (or another loopback).
    pub is_public: bool,
}

/// One row in `port_allocations` that needs an UPDATE to match reality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortUpdate {
    pub row_id: String,
    pub new_is_public: bool,
    pub new_public_port: Option<u16>,
}

/// Pure function — what UPDATEs would bring `allocations` into agreement
/// with `containers`. Side-effect-free and trivially unit-testable.
pub(crate) fn compute_sync_updates(
    allocations: &[PortAllocation],
    containers: &[ContainerBindings],
) -> Vec<PortUpdate> {
    let mut updates = Vec::new();

    // Single-container catalog services: every row has `compose_service =
    // NULL` and Docker reports one container (no compose label). Match by
    // container_port alone; the compose_service partition logic below would
    // refuse to match `None == None` consistently across these cases.
    let all_alloc_null = allocations.iter().all(|a| a.compose_service.is_none());
    let single_container = containers.len() == 1;
    let degraded_match = all_alloc_null && single_container;

    for alloc in allocations {
        let container = if degraded_match {
            containers.first()
        } else {
            containers
                .iter()
                .find(|c| c.compose_service.as_deref() == alloc.compose_service.as_deref())
        };

        let Some(container) = container else {
            // No container present for this compose service yet (e.g. stack
            // half-deployed). Leave the DB row alone — we can't tell what
            // Docker would do.
            continue;
        };

        let binding = container
            .bindings
            .iter()
            .find(|b| b.container_port as i64 == alloc.container_port);

        // For private rows (no host binding, or binding on 127.0.0.1) we
        // **preserve** the operator's saved `public_port` choice. They picked
        // it intentionally — toggling OFF/ON shouldn't lose it. Sync only
        // overwrites `public_port` when the container is currently public
        // (and even then, picks the actual host_port docker is using, since
        // that's what was just applied).
        let cur_public_port = alloc.public_port.map(|p| p as u16);
        let (new_is_public, new_public_port) = match binding {
            None => (false, cur_public_port),
            Some(b) if b.is_public => (true, Some(b.host_port)),
            Some(_) => (false, cur_public_port),
        };

        if new_is_public != alloc.is_public || new_public_port != cur_public_port {
            updates.push(PortUpdate {
                row_id: alloc.id.clone(),
                new_is_public,
                new_public_port,
            });
        }
    }

    updates
}

/// Best-effort: inspect every container of `service_id`'s compose project
/// and rewrite `port_allocations.is_public` / `public_port` so the DB
/// matches what `docker inspect` reports. Returns the number of rows
/// updated. Any Docker or DB failure is logged at warn and swallowed — the
/// caller (typically the resource-detail handler) just renders stale data
/// rather than 500-ing.
pub async fn sync_ports_from_docker(state: &AppState, service_id: &str) -> Result<usize> {
    let containers_list = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;

    // Container discovery mirrors recreate_with_port_bindings exactly:
    //   1. label pier.service.id (catalog-managed)
    //   2. services.container_id fallback (git-deployed, compose-template)
    //   3. compose project sibling expansion
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
        if let Some(cn) = cid_or_name.filter(|s| !s.is_empty()) {
            if let Ok(info) = state.docker.inspect_container(&cn, None).await {
                if let Some(id) = info.id {
                    target_ids.push(id);
                }
            }
        }
    }

    if target_ids.is_empty() {
        return Ok(0);
    }

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
            target_ids.extend(siblings);
        }
    }

    // Inspect every target container concurrently rather than sequentially —
    // on a multi-container stack this turns N round-trips to the Docker daemon
    // into ~1 round-trip of wall-clock latency. `Docker` is a cheap Arc clone.
    let inspects = target_ids.iter().map(|cid| {
        let docker = state.docker.clone();
        async move {
            let res = docker.inspect_container(cid, None).await;
            (cid, res)
        }
    });
    let inspect_results = futures_util::future::join_all(inspects).await;

    let mut container_bindings: Vec<ContainerBindings> = Vec::with_capacity(target_ids.len());
    for (cid, res) in inspect_results {
        let info = match res {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("sync_ports_from_docker: inspect {cid} failed: {e}");
                continue;
            }
        };
        let compose_service = info
            .config
            .as_ref()
            .and_then(|c| c.labels.as_ref())
            .and_then(|l| l.get("com.docker.compose.service"))
            .cloned();
        let hc = info.host_config.unwrap_or_default();
        let mut bindings: Vec<ContainerBinding> = Vec::new();
        if let Some(pb_map) = hc.port_bindings.as_ref() {
            for (key, entries_opt) in pb_map {
                let container_port: Option<u16> =
                    key.split('/').next().and_then(|s| s.parse().ok());
                let Some(cp) = container_port else { continue };
                let Some(entries) = entries_opt else { continue };
                for entry in entries {
                    let Some(hp_str) = entry.host_port.as_deref() else {
                        continue;
                    };
                    let Ok(hp) = hp_str.parse::<u16>() else {
                        continue;
                    };
                    let host_ip = entry.host_ip.as_deref().unwrap_or("");
                    let is_public = host_ip.is_empty() || host_ip == "0.0.0.0" || host_ip == "::";
                    bindings.push(ContainerBinding {
                        container_port: cp,
                        host_port: hp,
                        is_public,
                    });
                }
            }
        }
        container_bindings.push(ContainerBindings {
            compose_service,
            bindings,
        });
    }

    let allocations = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        crate::db::ports::get_ports(&db, service_id)?
    };

    let updates = compute_sync_updates(&allocations, &container_bindings);
    if updates.is_empty() {
        return Ok(0);
    }

    let n = updates.len();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        for u in &updates {
            if let Err(e) = db.execute(
                "UPDATE port_allocations SET is_public = ?1, public_port = ?2 WHERE id = ?3",
                rusqlite::params![
                    u.new_is_public as i64,
                    u.new_public_port.map(|p| p as i64),
                    u.row_id
                ],
            ) {
                tracing::warn!(
                    "sync_ports_from_docker: UPDATE row {row} failed: {e}",
                    row = u.row_id
                );
            }
        }
    }

    tracing::info!(
        "sync_ports_from_docker: service={service_id} synced {n} port row(s) to match Docker reality"
    );
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn alloc(
        id: &str,
        cp: i64,
        is_public: bool,
        public_port: Option<i64>,
        compose_service: Option<&str>,
    ) -> PortAllocation {
        PortAllocation {
            id: id.to_string(),
            service_id: "svc".to_string(),
            port_name: "primary".to_string(),
            host_port: cp,
            container_port: cp,
            protocol: "tcp".to_string(),
            is_public,
            public_port,
            created_at: String::new(),
            compose_service: compose_service.map(String::from),
        }
    }

    fn binding(cp: u16, hp: u16, is_public: bool) -> ContainerBinding {
        ContainerBinding {
            container_port: cp,
            host_port: hp,
            is_public,
        }
    }

    fn container(
        compose_service: Option<&str>,
        bindings: Vec<ContainerBinding>,
    ) -> ContainerBindings {
        ContainerBindings {
            compose_service: compose_service.map(String::from),
            bindings,
        }
    }

    #[test]
    fn sync_marks_public_when_docker_publishes_on_0000() {
        // flowfin case: DB says private, Docker publishes on 0.0.0.0 → mark public.
        let allocs = vec![alloc("r1", 3050, false, None, Some("api"))];
        let containers = vec![container(Some("api"), vec![binding(3050, 3050, true)])];
        let updates = compute_sync_updates(&allocs, &containers);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].row_id, "r1");
        assert!(updates[0].new_is_public);
        assert_eq!(updates[0].new_public_port, Some(3050));
    }

    #[test]
    fn sync_marks_private_when_docker_publishes_on_127001() {
        let allocs = vec![alloc("r1", 3050, true, Some(3050), Some("api"))];
        let containers = vec![container(Some("api"), vec![binding(3050, 3050, false)])];
        let updates = compute_sync_updates(&allocs, &containers);
        assert_eq!(updates.len(), 1);
        assert!(!updates[0].new_is_public);
        // Round 6: public_port preserved (operator's saved choice).
        assert_eq!(updates[0].new_public_port, Some(3050));
    }

    #[test]
    fn sync_preserves_public_port_for_private_row() {
        // Round 6: operator chose public_port=5432 last time they toggled
        // ON. Sync now sees a private container (no host binding). It must
        // NOT zero public_port — preserve the saved choice so the next
        // toggle ON uses 5432 again instead of defaulting to host_port.
        // Reverts the Round 4 behavior which surprised operators by
        // resetting their input on toggle OFF.
        let mut a = alloc("r1", 5432, false, Some(5432), Some("postgresql"));
        a.host_port = 10000;
        let allocs = vec![a];
        let containers = vec![container(Some("postgresql"), vec![])];
        let updates = compute_sync_updates(&allocs, &containers);
        assert!(
            updates.is_empty(),
            "sync must be a no-op when is_public matches and public_port should be preserved: {updates:?}"
        );
    }

    #[test]
    fn sync_marks_private_when_no_bindings_at_all() {
        // Container present but no published ports → DB row should reflect private.
        let allocs = vec![alloc("r1", 3054, true, Some(3054), Some("max-bot"))];
        let containers = vec![container(Some("max-bot"), vec![])];
        let updates = compute_sync_updates(&allocs, &containers);
        assert_eq!(updates.len(), 1);
        assert!(!updates[0].new_is_public);
        // Round 6: public_port preserved across toggle (saved choice).
        assert_eq!(updates[0].new_public_port, Some(3054));
    }

    #[test]
    fn sync_noop_when_db_already_matches_docker() {
        let allocs = vec![
            alloc("r1", 3050, true, Some(3050), Some("api")),
            alloc("r2", 3054, false, None, Some("max-bot")),
        ];
        let containers = vec![
            container(Some("api"), vec![binding(3050, 3050, true)]),
            container(Some("max-bot"), vec![]),
        ];
        let updates = compute_sync_updates(&allocs, &containers);
        assert!(updates.is_empty(), "expected no updates, got {updates:?}");
    }

    #[test]
    fn sync_partitions_bindings_by_compose_service() {
        // DB has api=private + max-bot=public; Docker has the opposite.
        // Both rows should be updated, no cross-contamination.
        let allocs = vec![
            alloc("r-api", 3050, false, None, Some("api")),
            alloc("r-bot", 3054, true, Some(3054), Some("max-bot")),
        ];
        let containers = vec![
            container(Some("api"), vec![binding(3050, 3050, true)]),
            container(Some("max-bot"), vec![]),
        ];
        let updates = compute_sync_updates(&allocs, &containers);
        assert_eq!(updates.len(), 2);
        let by_id: HashMap<&str, &PortUpdate> =
            updates.iter().map(|u| (u.row_id.as_str(), u)).collect();
        let api_u = by_id["r-api"];
        assert!(api_u.new_is_public);
        assert_eq!(api_u.new_public_port, Some(3050));
        let bot_u = by_id["r-bot"];
        assert!(!bot_u.new_is_public);
        // Round 6: bot's saved public_port=Some(3054) is preserved across the toggle.
        assert_eq!(bot_u.new_public_port, Some(3054));
    }

    #[test]
    fn sync_legacy_single_container_null_compose_service() {
        // Catalog Postgres: compose_service = NULL everywhere, single
        // container without compose labels. Must still sync via the
        // degraded single-container match path.
        let allocs = vec![alloc("r1", 5432, false, None, None)];
        let containers = vec![container(None, vec![binding(5432, 5432, true)])];
        let updates = compute_sync_updates(&allocs, &containers);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].new_is_public);
        assert_eq!(updates[0].new_public_port, Some(5432));
    }

    #[test]
    fn sync_skips_allocations_when_no_matching_container_running() {
        // Service rows exist (e.g. UPSERTed by deploy) but the stack isn't
        // up yet. Don't fabricate state — leave the DB alone.
        let allocs = vec![alloc("r1", 3050, true, Some(3050), Some("api"))];
        let containers: Vec<ContainerBindings> = vec![];
        let updates = compute_sync_updates(&allocs, &containers);
        assert!(updates.is_empty());
    }

    #[test]
    fn sync_empty_ip_treated_as_public() {
        // Docker default when YAML says "3050:3050" with no host-IP — the
        // entry comes back with HostIp = "" and Docker publishes on 0.0.0.0.
        // Same code path as explicit "0.0.0.0".
        let allocs = vec![alloc("r1", 3050, false, None, Some("api"))];
        let container_with_empty_ip = ContainerBindings {
            compose_service: Some("api".to_string()),
            bindings: vec![ContainerBinding {
                container_port: 3050,
                host_port: 3050,
                // The is_public flag is computed in sync_ports_from_docker
                // from host_ip; here we test that with is_public=true (empty
                // IP path) the update is applied.
                is_public: true,
            }],
        };
        let updates = compute_sync_updates(&allocs, &[container_with_empty_ip]);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].new_is_public);
    }
}
