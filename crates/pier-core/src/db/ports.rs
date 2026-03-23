use anyhow::{bail, Result};
use rusqlite::Connection;

use super::models::PortAllocation;

/// Allocate N free ports in the given range [start, end).
/// For each port, first tries to use the standard port (container_port == host_port),
/// e.g. 5432 for PostgreSQL. Falls back to pool if standard port is taken.
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

    for (_port_name, container_port) in port_specs {
        let standard = *container_port;
        if standard >= 1024 && !is_port_used(standard) && !newly_allocated.contains(&standard) {
            // Use standard port (e.g. 5432 for PostgreSQL)
            free.push(standard);
            newly_allocated.push(standard);
        } else {
            // Fall back to pool range
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
            "INSERT INTO port_allocations (id, service_id, port_name, host_port, container_port, protocol)
             VALUES (?1, ?2, ?3, ?4, ?5, 'tcp')",
            rusqlite::params![id, service_id, port_name, host_port as i64, *container_port as i64],
        )?;

        allocations.push(PortAllocation {
            id,
            service_id: service_id.to_string(),
            port_name: port_name.clone(),
            host_port: host_port as i64,
            container_port: *container_port as i64,
            protocol: "tcp".to_string(),
            created_at: String::new(),
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
pub fn get_ports(conn: &Connection, service_id: &str) -> Result<Vec<PortAllocation>> {
    let mut stmt = conn.prepare(
        "SELECT id, service_id, port_name, host_port, container_port, protocol, created_at
         FROM port_allocations WHERE service_id = ?1",
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
                created_at: row.get(6)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(ports)
}
