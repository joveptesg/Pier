//! Auth event audit trail. Records every significant credential / session
//! action into `auth_events` so an operator can answer "who, when, from
//! which IP" after the fact.
//!
//! Logging policy:
//!   - Never store passwords, TOTP codes, or recovery codes — neither in
//!     plaintext nor in any derived form. The `details` JSON is for non-secret
//!     classification only (reason codes, counts).
//!   - For login failures with an unknown username we deliberately do not
//!     record the supplied username — that would help an attacker enumerate
//!     valid accounts via the audit page itself.

use std::net::IpAddr;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;

/// Discrete event types we record. Adding a new variant requires updating
/// `event_kind()` so the DB writes a stable string.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthEvent {
    Setup,
    LoginSuccess,
    LoginFailure,
    LoginTotpRequired,
    LoginTotpSuccess,
    LoginTotpFailure,
    Logout,
    PasswordChange,
    TwoFaEnabled,
    TwoFaDisabled,
    SessionRevoked,
    SessionRevokedAll,
    // RBAC events
    UserInvited,
    UserInviteAccepted,
    UserDeleted,
    UserRoleChanged,
    ProjectMemberAdded,
    ProjectMemberRoleChanged,
    ProjectMemberRemoved,
    // Deploy events
    ServiceDeployed,
}

impl AuthEvent {
    /// Stable string identifier persisted in `auth_events.event_type`. Don't
    /// change these — UIs filter on them and old rows would lose their labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::LoginSuccess => "login_success",
            Self::LoginFailure => "login_failure",
            Self::LoginTotpRequired => "login_totp_required",
            Self::LoginTotpSuccess => "login_totp_success",
            Self::LoginTotpFailure => "login_totp_failure",
            Self::Logout => "logout",
            Self::PasswordChange => "password_change",
            Self::TwoFaEnabled => "two_fa_enabled",
            Self::TwoFaDisabled => "two_fa_disabled",
            Self::SessionRevoked => "session_revoked",
            Self::SessionRevokedAll => "session_revoked_all",
            Self::UserInvited => "user.invited",
            Self::UserInviteAccepted => "user.invite_accepted",
            Self::UserDeleted => "user.deleted",
            Self::UserRoleChanged => "user.role_changed",
            Self::ProjectMemberAdded => "project.member_added",
            Self::ProjectMemberRoleChanged => "project.member_role_changed",
            Self::ProjectMemberRemoved => "project.member_removed",
            Self::ServiceDeployed => "service.deployed",
        }
    }
}

/// Persist one audit event. Errors are logged at warn level but never
/// returned — losing an audit row should not fail the underlying auth action.
pub fn log(
    state: &SharedState,
    event: AuthEvent,
    user_id: Option<&str>,
    ip: Option<IpAddr>,
    user_agent: Option<&str>,
    details: Option<serde_json::Value>,
) {
    let id = uuid::Uuid::new_v4().to_string();
    let ip_str = ip.map(|i| i.to_string());
    let details_str = details.as_ref().map(|v| v.to_string());

    let result = {
        let db = match state.db.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("audit log skipped (db poisoned): {e}");
                return;
            }
        };
        db.execute(
            "INSERT INTO auth_events (id, user_id, event_type, ip, user_agent, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, user_id, event.as_str(), ip_str, user_agent, details_str],
        )
    };

    if let Err(e) = result {
        tracing::warn!("audit log insert failed for {:?}: {e}", event);
    }
}

/// Drop events older than the configured retention. Sensitive events
/// (`PasswordChange`, `TwoFaEnabled`, `TwoFaDisabled`, `Setup`) survive
/// longer — the threshold is `retention_days_sensitive`.
///
/// Returns (rows deleted normal, rows deleted sensitive).
pub fn retention_sweep(state: &SharedState) -> (usize, usize) {
    let (days, days_sensitive) = read_retention(state);
    let mut deleted_normal = 0usize;
    let mut deleted_sensitive = 0usize;

    let sensitive_kinds = [
        "password_change",
        "two_fa_enabled",
        "two_fa_disabled",
        "setup",
    ];
    let placeholders = sensitive_kinds
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");

    if let Ok(db) = state.db.lock() {
        // Normal events: everything not in the sensitive list, older than `days`.
        let sql_normal = format!(
            "DELETE FROM auth_events
             WHERE event_type NOT IN ({placeholders})
               AND created_at < datetime('now', ?{})",
            sensitive_kinds.len() + 1
        );
        let mut args: Vec<String> = sensitive_kinds.iter().map(|s| s.to_string()).collect();
        args.push(format!("-{days} days"));
        let params_iter: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        match db.execute(&sql_normal, params_iter.as_slice()) {
            Ok(n) => deleted_normal = n,
            Err(e) => tracing::warn!("audit retention (normal) failed: {e}"),
        }

        let sql_sensitive = format!(
            "DELETE FROM auth_events
             WHERE event_type IN ({placeholders})
               AND created_at < datetime('now', ?{})",
            sensitive_kinds.len() + 1
        );
        let mut args2: Vec<String> = sensitive_kinds.iter().map(|s| s.to_string()).collect();
        args2.push(format!("-{days_sensitive} days"));
        let params_iter2: Vec<&dyn rusqlite::ToSql> =
            args2.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        match db.execute(&sql_sensitive, params_iter2.as_slice()) {
            Ok(n) => deleted_sensitive = n,
            Err(e) => tracing::warn!("audit retention (sensitive) failed: {e}"),
        }
    }

    (deleted_normal, deleted_sensitive)
}

fn read_retention(state: &SharedState) -> (u32, u32) {
    let db = match state.db.lock() {
        Ok(g) => g,
        Err(_) => return (90, 365),
    };
    let get = |key: &str, default: u32| -> u32 {
        db.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
    };
    (
        get("audit.retention_days", 90),
        get("audit.retention_days_sensitive", 365),
    )
}
