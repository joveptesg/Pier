//! High-level permission checks.
//!
//! Handlers should reach for [`can`] (or the per-route guards in
//! [`super::guards`]) rather than hand-rolling role comparisons. That keeps
//! the policy surface in one file and makes the route-to-permission mapping
//! grep-able.

// `Permission` variants for project-scoped routes are wired up incrementally
// — Stage 1 ships only the guards used by user-management endpoints. Allow
// dead_code so the enum can carry the full vocabulary today.
#![allow(dead_code)]

use rusqlite::Connection;

use super::membership;
use super::roles::{GlobalRole, ProjectRole};
use crate::auth::middleware::AuthUser;

/// A discrete authorisation question.
#[derive(Clone, Debug)]
pub enum Permission {
    /// Create / delete users, invite, change passwords (not own).
    ManageUsers,
    /// Assign / revoke the Owner global role — Owner-only.
    ChangeGlobalRoles,
    /// Create / delete / rotate / promote servers.
    ManageServers,
    /// Read system metrics, audit log, system info.
    ViewSystem,
    /// Create new projects at the system level.
    CreateProjects,
    /// Read a specific project (Viewer+ or global Admin+).
    ViewProject(String),
    /// Edit a specific project's services, env, deployments (Editor+).
    EditProject(String),
    /// Admin a specific project — manage its membership (Project Admin+).
    AdminProject(String),
    /// Federation grants, mesh, cross-core trust — Owner-only.
    ManageFederation,
}

/// Returns true if the user is allowed to perform the given action.
///
/// Peer-authenticated requests (`AuthUser.is_peer == true`) are treated as
/// Admin-equivalent for resource operations but never granted user-management
/// or federation permissions — peers should not edit local user records.
pub fn can(user: &AuthUser, perm: &Permission, conn: &Connection) -> bool {
    if user.is_peer {
        return matches!(
            perm,
            Permission::ViewSystem
                | Permission::ViewProject(_)
                | Permission::EditProject(_)
                | Permission::AdminProject(_)
                | Permission::CreateProjects
                | Permission::ManageServers
        );
    }

    match perm {
        Permission::ChangeGlobalRoles | Permission::ManageFederation => {
            user.global_role == GlobalRole::Owner
        }
        Permission::ManageUsers
        | Permission::ManageServers
        | Permission::ViewSystem
        | Permission::CreateProjects => user.global_role.at_least(GlobalRole::Admin),

        Permission::ViewProject(pid) => {
            if user.global_role.at_least(GlobalRole::Admin) {
                return true;
            }
            membership::role_for(conn, &user.id, pid)
                .ok()
                .flatten()
                .map(|r| r.at_least(ProjectRole::Viewer))
                .unwrap_or(false)
        }
        Permission::EditProject(pid) => {
            if user.global_role.at_least(GlobalRole::Admin) {
                return true;
            }
            membership::role_for(conn, &user.id, pid)
                .ok()
                .flatten()
                .map(|r| r.at_least(ProjectRole::Editor))
                .unwrap_or(false)
        }
        Permission::AdminProject(pid) => {
            if user.global_role.at_least(GlobalRole::Admin) {
                return true;
            }
            membership::role_for(conn, &user.id, pid)
                .ok()
                .flatten()
                .map(|r| r.at_least(ProjectRole::Admin))
                .unwrap_or(false)
        }
    }
}
