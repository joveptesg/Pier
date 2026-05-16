//! SQLite-backed membership lookups for project-scoped RBAC.
//!
//! No in-process cache for now: each project guard does one indexed
//! `project_members` lookup per request, which is a sub-millisecond
//! operation on SQLite (WAL mode + `idx_project_members_user`). If the
//! request rate ever justifies caching we can plug in `moka` here without
//! changing call-sites.

// Several helpers below are called only by handlers that ship in Stages 2/3
// of the RBAC roll-out (project-scoped enforcement on `/resources/**` and
// `/team`-side member listings). Allowing dead_code keeps them present in
// the public API surface without earning a warning today.
#![allow(dead_code)]

use anyhow::Result;
use rusqlite::Connection;

use super::roles::ProjectRole;

/// One row of `project_members` projected to the role enum.
#[derive(Clone, Debug)]
pub struct ProjectMember {
    pub project_id: String,
    pub user_id: String,
    pub role: ProjectRole,
}

/// Look up a user's role in a single project. Returns `None` if they are
/// not a member.
pub fn role_for(conn: &Connection, user_id: &str, project_id: &str) -> Result<Option<ProjectRole>> {
    let mut stmt = conn.prepare(
        "SELECT project_role FROM project_members
         WHERE user_id = ?1 AND project_id = ?2",
    )?;
    let role_str: Option<String> = stmt
        .query_row([user_id, project_id], |row| row.get::<_, String>(0))
        .ok();
    Ok(role_str.and_then(|s| ProjectRole::parse(&s)))
}

/// List all projects this user has explicit membership in, with their role.
/// Used by the `/team` UI and by handlers that need to filter project lists
/// for non-admin users.
pub fn list_for_user(conn: &Connection, user_id: &str) -> Result<Vec<ProjectMember>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, project_role FROM project_members
         WHERE user_id = ?1",
    )?;
    let rows = stmt
        .query_map([user_id], |row| {
            let pid: String = row.get(0)?;
            let role_s: String = row.get(1)?;
            Ok((pid, role_s))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(pid, role_s)| {
            ProjectRole::parse(&role_s).map(|role| ProjectMember {
                project_id: pid,
                user_id: user_id.to_string(),
                role,
            })
        })
        .collect();
    Ok(rows)
}

/// List every member of a given project, ordered by role rank descending
/// (admin → editor → viewer).
pub fn list_for_project(conn: &Connection, project_id: &str) -> Result<Vec<ProjectMember>> {
    let mut stmt = conn.prepare(
        "SELECT user_id, project_role FROM project_members
         WHERE project_id = ?1
         ORDER BY
            CASE project_role
                WHEN 'admin'  THEN 0
                WHEN 'editor' THEN 1
                WHEN 'viewer' THEN 2
                ELSE 3
            END,
            added_at ASC",
    )?;
    let rows = stmt
        .query_map([project_id], |row| {
            let uid: String = row.get(0)?;
            let role_s: String = row.get(1)?;
            Ok((uid, role_s))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(uid, role_s)| {
            ProjectRole::parse(&role_s).map(|role| ProjectMember {
                project_id: project_id.to_string(),
                user_id: uid,
                role,
            })
        })
        .collect();
    Ok(rows)
}

/// Count Project Admins. Used to enforce the "≥1 Project Admin per project"
/// invariant on member-role changes and removals.
pub fn count_admins(conn: &Connection, project_id: &str) -> Result<u32> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM project_members
         WHERE project_id = ?1 AND project_role = 'admin'",
        [project_id],
        |row| row.get(0),
    )?;
    Ok(count as u32)
}

/// Resolve the project a given resource (service row) belongs to. Returns
/// `None` for orphan services (project_id is nullable on services).
///
/// Used by the project-scoped guard to translate `/resources/{id}` requests
/// into a project membership check.
pub fn project_for_resource(conn: &Connection, resource_id: &str) -> Result<Option<String>> {
    let pid: Option<String> = conn
        .query_row(
            "SELECT project_id FROM services WHERE id = ?1",
            [resource_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    Ok(pid)
}
