//! Role-based access control.
//!
//! Pier's authorisation model has two scopes:
//!
//! * **Global roles** ([`GlobalRole`]) carried on every authenticated user.
//!   `Owner` is reserved for the installer (≥1 enforced), `Admin` can manage
//!   users / servers / system settings, `User` is a regular member who only
//!   sees what they've been explicitly granted.
//!
//! * **Project roles** ([`ProjectRole`]) stored in the `project_members`
//!   table. They scope a user's reach to specific projects without giving
//!   them global power. Global `Owner` / `Admin` bypass these rows.
//!
//! Higher-level helpers live in [`policy`]; per-route axum guards live in
//! [`guards`]; SQLite-backed membership lookups (with an in-process cache)
//! live in [`membership`].

pub mod guards;
pub mod membership;
pub mod policy;
pub mod roles;

#[allow(unused_imports)]
pub use guards::{enforce_project_role, enforce_resource_role, ProjectMembership};
#[allow(unused_imports)]
pub use roles::{GlobalRole, ProjectRole};
