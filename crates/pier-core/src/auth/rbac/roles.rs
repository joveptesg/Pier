//! Role enums and ordering.
//!
//! Both [`GlobalRole`] and [`ProjectRole`] are linearly ordered: a higher
//! rank includes every permission of the lower ones. Compare via
//! [`GlobalRole::at_least`] / [`ProjectRole::at_least`] rather than `==` so
//! adding a role between existing levels doesn't silently break call-sites.

use serde::{Deserialize, Serialize};

/// System-wide role carried on every authenticated user.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GlobalRole {
    /// Installer / instance owner. Exactly one is required at all times —
    /// the last Owner cannot be demoted or deleted.
    Owner,
    /// Full system access except the Owner-only knobs (assigning Owner role,
    /// federation grants, mesh).
    Admin,
    /// Regular authenticated user. Sees only projects they're a member of
    /// and the global read-only endpoints (`/servers` list, system metrics).
    User,
}

impl GlobalRole {
    pub fn rank(self) -> u8 {
        match self {
            GlobalRole::Owner => 3,
            GlobalRole::Admin => 2,
            GlobalRole::User => 1,
        }
    }

    /// Returns true if `self` includes every privilege of `other`.
    pub fn at_least(self, other: GlobalRole) -> bool {
        self.rank() >= other.rank()
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(GlobalRole::Owner),
            "admin" => Some(GlobalRole::Admin),
            "user" => Some(GlobalRole::User),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GlobalRole::Owner => "owner",
            GlobalRole::Admin => "admin",
            GlobalRole::User => "user",
        }
    }
}

/// Role of a user within a single project (`project_members.project_role`).
#[derive(Copy, Clone, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectRole {
    /// Manages membership + everything Editor can do.
    Admin,
    /// Deploy, redeploy, change env/domains/settings, restart.
    Editor,
    /// Read-only: configs, logs, metrics, deployment history.
    Viewer,
}

impl ProjectRole {
    pub fn rank(self) -> u8 {
        match self {
            ProjectRole::Admin => 3,
            ProjectRole::Editor => 2,
            ProjectRole::Viewer => 1,
        }
    }

    pub fn at_least(self, other: ProjectRole) -> bool {
        self.rank() >= other.rank()
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(ProjectRole::Admin),
            "editor" => Some(ProjectRole::Editor),
            "viewer" => Some(ProjectRole::Viewer),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProjectRole::Admin => "admin",
            ProjectRole::Editor => "editor",
            ProjectRole::Viewer => "viewer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_role_ordering() {
        assert!(GlobalRole::Owner.at_least(GlobalRole::Admin));
        assert!(GlobalRole::Owner.at_least(GlobalRole::User));
        assert!(GlobalRole::Admin.at_least(GlobalRole::User));
        assert!(!GlobalRole::Admin.at_least(GlobalRole::Owner));
        assert!(!GlobalRole::User.at_least(GlobalRole::Admin));
    }

    #[test]
    fn project_role_ordering() {
        assert!(ProjectRole::Admin.at_least(ProjectRole::Editor));
        assert!(ProjectRole::Editor.at_least(ProjectRole::Viewer));
        assert!(!ProjectRole::Viewer.at_least(ProjectRole::Editor));
    }

    #[test]
    fn round_trip_strings() {
        for r in [GlobalRole::Owner, GlobalRole::Admin, GlobalRole::User] {
            assert_eq!(GlobalRole::parse(r.as_str()), Some(r));
        }
        for r in [ProjectRole::Admin, ProjectRole::Editor, ProjectRole::Viewer] {
            assert_eq!(ProjectRole::parse(r.as_str()), Some(r));
        }
        assert!(GlobalRole::parse("garbage").is_none());
        assert!(ProjectRole::parse("garbage").is_none());
    }
}
