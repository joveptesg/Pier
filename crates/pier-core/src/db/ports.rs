use anyhow::{bail, Result};
use rusqlite::Connection;

use super::models::PortAllocation;

/// Allocate N free ports in the given range [start, end).
/// For each port, first tries to use the standard port (container_port == host_port),
/// e.g. 5432 for PostgreSQL. Falls back to pool if standard port is taken.
/// `is_public` controls whether the port binds to 0.0.0.0 (true) or 127.0.0.1 (false).
pub fn allocate_ports(
    conn: &Connection,
    service_id: &str,
    port_specs: &[(String, u16)], // (port_name, container_port)
    start: u16,
    end: u16,
) -> Result<Vec<PortAllocation>> {
    let count = port_specs.len();
    if count == 0 {
        return Ok(Vec::new());
    }

    // Get ALL currently allocated ports (not just in range — needed for standard port check)
    let mut stmt = conn.prepare("SELECT host_port FROM port_allocations ORDER BY host_port")?;
    let all_used: Vec<u16> = stmt
        .query_map([], |row| row.get::<_, i64>(0).map(|p| p as u16))?
        .filter_map(|r| r.ok())
        .collect();

    let is_port_used = |p: u16| all_used.binary_search(&p).is_ok();

    // For each port spec, try standard port first, then fall back to pool
    let mut free = Vec::with_capacity(count);
    let mut newly_allocated = Vec::new(); // track ports we've already picked in this batch

    for (_port_name, _container_port) in port_specs {
        // Always allocate from pool range (10000+) to avoid conflicts with
        // Traefik TCP proxy which needs standard ports (5432, 3306, etc.)
        let mut port = start;
        let mut found = false;
        while port < end {
            if !is_port_used(port) && !newly_allocated.contains(&port) {
                free.push(port);
                newly_allocated.push(port);
                found = true;
                break;
            }
            port += 1;
        }
        if !found {
            bail!("Not enough free ports in range {start}-{end}");
        }
    }

    if free.len() < count {
        bail!(
            "Not enough free ports: need {count}, found {} in range {start}-{end}",
            free.len()
        );
    }

    // Insert allocations
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
}
