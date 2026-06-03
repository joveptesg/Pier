//! Declared service dependency graph (Layer C of monorepo selective redeploy).
//!
//! Operators declare "service A depends_on service B" edges in the
//! `service_dependencies` table. When B is (re)deployed because a push touched
//! its watched paths, A must redeploy too. This module computes that
//! reverse-dependency closure for the webhook fan-out.
//!
//! This is an EXPLICIT, operator-declared graph — debuggable and opt-in — NOT
//! nx/turbo-style content-hash inference. Teams using nx/turbo should instead
//! drive the per-service CI deploy API (`POST /services/{id}/deploy`), which
//! already knows the affected-with-dependents set; Layer C is for everyone
//! else. An empty table reproduces the pre-graph behavior exactly.

use std::collections::{BTreeSet, VecDeque};

use anyhow::Result;
use rusqlite::Connection;

/// Given the directly-affected service ids (`seeds`), return the FULL set to
/// redeploy: the seeds plus the transitive closure of every service that
/// declares a dependency on them (reverse edges).
///
/// Cycle-safe via a visited set — a node already seen is never re-enqueued, so
/// `A→B→A` terminates. The returned `BTreeSet` INCLUDES the seeds and is
/// deterministically ordered (stable fan-out order, easy to test). An empty
/// `service_dependencies` table returns exactly the seeds.
pub fn expand_with_dependents(conn: &Connection, seeds: &[String]) -> Result<BTreeSet<String>> {
    let mut visited: BTreeSet<String> = seeds.iter().cloned().collect();
    let mut queue: VecDeque<String> = seeds.iter().cloned().collect();

    // Safety cap against a pathological graph: a legitimate closure can never
    // exceed the total number of services. Falls back to "no cap" if the count
    // can't be read (the visited set still bounds the work via dedup).
    let max = service_count(conn).unwrap_or(usize::MAX);

    let mut stmt = conn
        .prepare("SELECT service_id FROM service_dependencies WHERE depends_on_service_id = ?1")?;

    while let Some(current) = queue.pop_front() {
        if visited.len() > max {
            break;
        }
        let dependents = stmt.query_map([&current], |row| row.get::<_, String>(0))?;
        for dep in dependents {
            let dep = dep?;
            // visited.insert returns true only for newly-seen ids → cycle guard.
            if visited.insert(dep.clone()) {
                queue.push_back(dep);
            }
        }
    }

    Ok(visited)
}

fn service_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM services", [], |r| r.get(0))?;
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory DB with just the columns the closure walk touches.
    fn db_with(edges: &[(&str, &str)], services: &[&str]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE services (id TEXT PRIMARY KEY);
             CREATE TABLE service_dependencies (
                 id TEXT PRIMARY KEY,
                 service_id TEXT NOT NULL,
                 depends_on_service_id TEXT NOT NULL
             );",
        )
        .unwrap();
        for s in services {
            conn.execute("INSERT INTO services (id) VALUES (?1)", [s])
                .unwrap();
        }
        for (i, (svc, dep)) in edges.iter().enumerate() {
            conn.execute(
                "INSERT INTO service_dependencies (id, service_id, depends_on_service_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![format!("e{i}"), svc, dep],
            )
            .unwrap();
        }
        conn
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn seeds(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_graph_returns_only_seeds() {
        let conn = db_with(&[], &["a", "b"]);
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["a"])).unwrap(),
            set(&["a"])
        );
    }

    #[test]
    fn linear_chain() {
        // b depends_on a, c depends_on b
        let conn = db_with(&[("b", "a"), ("c", "b")], &["a", "b", "c"]);
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["a"])).unwrap(),
            set(&["a", "b", "c"])
        );
        // Seeding the middle only pulls its dependents.
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["b"])).unwrap(),
            set(&["b", "c"])
        );
        // A leaf pulls nothing new.
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["c"])).unwrap(),
            set(&["c"])
        );
    }

    #[test]
    fn diamond() {
        // b->a, c->a, d->b, d->c  (d depends on b and c; both depend on a)
        let conn = db_with(
            &[("b", "a"), ("c", "a"), ("d", "b"), ("d", "c")],
            &["a", "b", "c", "d"],
        );
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["a"])).unwrap(),
            set(&["a", "b", "c", "d"])
        );
    }

    #[test]
    fn cycle_terminates() {
        // Mutual dependency a<->b must terminate, not loop forever.
        let conn = db_with(&[("a", "b"), ("b", "a")], &["a", "b"]);
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["a"])).unwrap(),
            set(&["a", "b"])
        );
    }

    #[test]
    fn multiple_seeds_union() {
        let conn = db_with(&[("b", "a"), ("z", "y")], &["a", "b", "y", "z"]);
        assert_eq!(
            expand_with_dependents(&conn, &seeds(&["a", "y"])).unwrap(),
            set(&["a", "b", "y", "z"])
        );
    }
}
