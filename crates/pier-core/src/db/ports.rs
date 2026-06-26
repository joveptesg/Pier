use anyhow::{bail, Result};
use rusqlite::Connection;

use super::models::PortAllocation;

/// Allocate N free host ports for a service, one per `port_specs` entry.
///
/// Per-port resolution order:
///   1. If `host_port_overrides[i] = Some(p)` and `p` is free → use `p`. If
///      `p` is taken, bail — the operator explicitly asked for it.
///   2. Otherwise, try the **standard port** = container_port (e.g. 5432 for
///      Postgres). If free and ≥ 1024 (avoid privileged-port range) → use it.
///      This gives `psql -h host -p 5432` ergonomics on first deploy.
///   3. Otherwise, scan `[start, end)` for the next free port and pick it.
///
/// Earlier this function always picked from `[start, end)` to reserve
/// standard ports for an in-process Traefik TCP proxy. After the Round 1-2
/// public-access refactor, raw-TCP exposure is direct docker port binding
/// (not Traefik), so there's no reason to lock standard ports any more.
///
/// `host_port_overrides`, when non-empty, must have exactly `port_specs.len()`
/// entries — one per port spec. Pass an empty slice for "auto for all".
///
/// `pool_only` skips step 2 (the standard-port shortcut) and allocates strictly
/// from `[start, end)`. Cross-server CLUSTER nodes MUST use this: their published
/// ports have to land in the mesh firewall band (the pool range), otherwise a
/// node can't reach its own published port for replica-set/sentinel self-checks.
pub fn allocate_ports(
    conn: &Connection,
    service_id: &str,
    port_specs: &[(String, u16)], // (port_name, container_port)
    start: u16,
    end: u16,
    host_port_overrides: &[Option<u16>],
    pool_only: bool,
) -> Result<Vec<PortAllocation>> {
    let count = port_specs.len();
    if count == 0 {
        return Ok(Vec::new());
    }
    if !host_port_overrides.is_empty() && host_port_overrides.len() != count {
        bail!(
            "host_port_overrides length ({}) must match port_specs length ({count})",
            host_port_overrides.len()
        );
    }

    // Snapshot every currently-allocated host port (across all services) so
    // step 2 can check "is 5432 free?" without another query per spec.
    let mut stmt = conn.prepare("SELECT host_port FROM port_allocations ORDER BY host_port")?;
    let all_used: Vec<u16> = stmt
        .query_map([], |row| row.get::<_, i64>(0).map(|p| p as u16))?
        .filter_map(|r| r.ok())
        .collect();

    let is_port_used = |p: u16| all_used.binary_search(&p).is_ok();

    let mut free = Vec::with_capacity(count);
    let mut newly_allocated: Vec<u16> = Vec::with_capacity(count);

    for (i, (_port_name, container_port)) in port_specs.iter().enumerate() {
        let override_value = host_port_overrides.get(i).copied().flatten();
        let chosen = match override_value {
            Some(requested) => {
                if is_port_used(requested) || newly_allocated.contains(&requested) {
                    bail!(
                        "Requested host port {requested} is already in use by another \
                         allocation; pick a different one or leave empty for auto."
                    );
                }
                requested
            }
            None => {
                let standard = *container_port;
                if !pool_only
                    && standard >= 1024
                    && !is_port_used(standard)
                    && !newly_allocated.contains(&standard)
                {
                    standard
                } else {
                    // Pool fallback — scan [start, end) for the next free slot.
                    let mut p = start;
                    loop {
                        if p >= end {
                            bail!("Not enough free ports in pool range {start}-{end}");
                        }
                        if !is_port_used(p) && !newly_allocated.contains(&p) {
                            break p;
                        }
                        p += 1;
                    }
                }
            }
        };
        free.push(chosen);
        newly_allocated.push(chosen);
    }

    let mut allocations = Vec::with_capacity(count);
    for (i, (port_name, container_port)) in port_specs.iter().enumerate() {
        let id = uuid::Uuid::new_v4().to_string();
        let host_port = free[i];

        conn.execute(
            "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol, is_public)
             VALUES (?1, ?2, ?3, ?4, ?5, 'tcp', 0)",
            rusqlite::params![id, service_id, port_name, host_port as i64, *container_port as i64],
        )?;

        allocations.push(PortAllocation {
            id,
            service_id: service_id.to_string(),
            port_name: port_name.clone(),
            host_port: host_port as i64,
            container_port: *container_port as i64,
            protocol: "tcp".to_string(),
            is_public: false,
            public_port: None,
            created_at: String::new(),
            compose_service: None,
        });
    }

    Ok(allocations)
}

/// Free all port allocations for a service.
pub fn free_ports(conn: &Connection, service_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM port_allocations WHERE service_id = ?1",
        [service_id],
    )?;
    Ok(())
}

/// Get all port allocations for a service.
///
/// Rows are returned in a deterministic order: by `compose_service` (NULLs
/// first — single-service catalog deployments) then by `port_name`. Without
/// the ORDER BY clause SQLite returns rows in `rowid` order, and after
/// commit b84aa79 (UPSERT keyed by `compose_service`) the rowid of existing
/// multi-service compose stacks no longer matches the order they appear in
/// `docker-compose.yml` — the UI's Ports list visibly reshuffled between
/// loads. Alphabetical by compose_service is stable and good enough for the
/// common case; callers that need YAML-declaration order should re-sort
/// against `services.compose_content` after the fact.
pub fn get_ports(conn: &Connection, service_id: &str) -> Result<Vec<PortAllocation>> {
    let mut stmt = conn.prepare(
        "SELECT id, service_id, port_name, host_port, container_port, protocol, is_public, public_port, created_at, compose_service
         FROM port_allocations WHERE service_id = ?1
         ORDER BY compose_service IS NULL DESC, compose_service ASC, port_name ASC",
    )?;

    let ports = stmt
        .query_map([service_id], |row| {
            Ok(PortAllocation {
                id: row.get(0)?,
                service_id: row.get(1)?,
                port_name: row.get(2)?,
                host_port: row.get(3)?,
                container_port: row.get(4)?,
                protocol: row.get(5)?,
                is_public: row.get::<_, i64>(6)? != 0,
                public_port: row.get(7)?,
                created_at: row.get(8)?,
                compose_service: row.get(9)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(ports)
}

/// Set public visibility for all ports of a service.
#[allow(dead_code)]
pub fn set_ports_public(conn: &Connection, service_id: &str, is_public: bool) -> Result<()> {
    conn.execute(
        "UPDATE port_allocations SET is_public = ?1 WHERE service_id = ?2",
        rusqlite::params![is_public as i64, service_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        schema::run_migrations(&conn).expect("migrations");
        conn.execute(
            "INSERT INTO services (id, name, service_type) VALUES ('svc-1', 'flowfin', 'compose')",
            [],
        )
        .expect("seed service");
        conn
    }

    fn insert_alloc(
        conn: &Connection,
        id: &str,
        port_name: &str,
        host: i64,
        compose_service: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO port_allocations \
             (id, service_id, port_name, host_port, container_port, protocol, is_public, public_port, compose_service) \
             VALUES (?1, 'svc-1', ?2, ?3, ?3, 'tcp', 0, NULL, ?4)",
            rusqlite::params![id, port_name, host, compose_service],
        )
        .expect("insert alloc");
    }

    #[test]
    fn get_ports_stable_alphabetical_order_by_compose_service() {
        let conn = test_conn();
        // Insert in REVERSE order so naive rowid-sort would put max-bot first.
        // The ORDER BY in get_ports must override that and return api first.
        insert_alloc(&conn, "p2", "primary", 3054, Some("max-bot"));
        insert_alloc(&conn, "p1", "primary", 3050, Some("api"));

        let rows = get_ports(&conn, "svc-1").expect("get_ports");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].compose_service.as_deref(), Some("api"));
        assert_eq!(rows[0].host_port, 3050);
        assert_eq!(rows[1].compose_service.as_deref(), Some("max-bot"));
        assert_eq!(rows[1].host_port, 3054);
    }

    #[test]
    fn get_ports_legacy_null_compose_service_sorted_by_port_name() {
        let conn = test_conn();
        // Single-container service (catalog or pre-b84aa79): every row has
        // compose_service = NULL. Fall back to port_name for stable order.
        insert_alloc(&conn, "p1", "primary", 4471, None);
        insert_alloc(&conn, "p2", "port-1", 1883, None);

        let rows = get_ports(&conn, "svc-1").expect("get_ports");
        assert_eq!(rows.len(), 2);
        // "port-1" sorts before "primary" alphabetically.
        assert_eq!(rows[0].port_name, "port-1");
        assert_eq!(rows[1].port_name, "primary");
    }

    // ─── allocate_ports: standard-first + optional override ──────────────

    fn seed_alt_service(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO services (id, name, service_type) VALUES (?1, ?1, 'compose')",
            [id],
        )
        .expect("seed alt service");
    }

    #[test]
    fn allocate_standard_port_when_free() {
        // Brand-new postgres: container_port=5432, no one holds 5432 yet →
        // host_port=5432 (NOT a number from the 10000+ pool). Round 5 fix.
        let conn = test_conn();
        let specs = vec![("primary".to_string(), 5432u16)];
        let allocs =
            allocate_ports(&conn, "svc-1", &specs, 10000, 20000, &[], false).expect("allocate");
        assert_eq!(allocs.len(), 1);
        assert_eq!(
            allocs[0].host_port, 5432,
            "standard port must be picked when free"
        );
    }

    #[test]
    fn allocate_falls_back_to_pool_when_standard_taken() {
        // Two postgres services in a row. First gets 5432; second can't, so
        // falls back to the first free slot in the pool (10000).
        let conn = test_conn();
        seed_alt_service(&conn, "svc-2");

        let specs = vec![("primary".to_string(), 5432u16)];
        let first = allocate_ports(&conn, "svc-1", &specs, 10000, 20000, &[], false).unwrap();
        assert_eq!(first[0].host_port, 5432);

        let second = allocate_ports(&conn, "svc-2", &specs, 10000, 20000, &[], false).unwrap();
        assert_eq!(
            second[0].host_port, 10000,
            "second standard-port request must fall through to pool start"
        );
    }

    #[test]
    fn allocate_honours_explicit_override() {
        // Operator explicitly asked for host_port=15432 → it's free, give it.
        let conn = test_conn();
        let specs = vec![("primary".to_string(), 5432u16)];
        let allocs = allocate_ports(&conn, "svc-1", &specs, 10000, 20000, &[Some(15432)], false)
            .expect("allocate");
        assert_eq!(allocs[0].host_port, 15432);
    }

    #[test]
    fn allocate_errors_when_override_taken() {
        // Standard 5432 already taken by an existing allocation; operator
        // explicitly requests 5432 for a NEW service → bail (not silently
        // fall through to pool — they asked for THIS port).
        let conn = test_conn();
        seed_alt_service(&conn, "svc-2");
        let specs = vec![("primary".to_string(), 5432u16)];
        allocate_ports(&conn, "svc-1", &specs, 10000, 20000, &[], false).unwrap();

        let err = allocate_ports(&conn, "svc-2", &specs, 10000, 20000, &[Some(5432)], false)
            .expect_err("must bail");
        assert!(
            err.to_string().contains("already in use"),
            "error must mention the conflict, got: {err}"
        );
    }

    #[test]
    fn allocate_skips_standard_below_1024() {
        // Privileged ports (<1024) need root to bind. Even if container_port=80
        // is "free" by DB lookup, don't try to give it as host_port — go to
        // the pool. Otherwise docker-proxy startup would fail on a non-root
        // pier-core.
        let conn = test_conn();
        let specs = vec![("primary".to_string(), 80u16)];
        let allocs = allocate_ports(&conn, "svc-1", &specs, 10000, 20000, &[], false).unwrap();
        assert!(
            allocs[0].host_port >= 10000,
            "privileged container port must NOT be picked; got {}",
            allocs[0].host_port
        );
    }
}
