//! Stateless service migration (Этап 4).
//!
//! `POST /api/v1/stacks/{id}/migrate { target_server_id }` moves a
//! locally-owned compose stack to a federated peer. Only stacks with
//! NO named volumes qualify — anything stateful belongs to the
//! deferred Этап 4.B (volume snapshot) or 4.C (DB-aware) tracks.
//!
//! This file currently exposes only:
//! - [`is_stack_stateless`] — the rule the orchestrator and the UI
//!   use to decide whether to offer the "Move to..." button.
//! - [`acquire_migration_lock`] / [`release_migration_lock`] — atomic
//!   takes on `services.migration_in_progress` so two operators
//!   clicking at the same time both see the same answer (winner
//!   proceeds, loser sees 409).
//!
//! The orchestrator handler that actually drives the migration moves
//! in next (Этап 4.2).

// Consumers land in 4.2 (orchestrator handler) and 4.3 (UI eligibility
// check). Re-evaluate this allow once those ship.
#![allow(dead_code)]

use rusqlite::Connection;

use crate::error::{AppError, AppResult};

/// Detailed result of a statelessness check. The error string is
/// surfaced to the operator verbatim so they understand why a Move
/// button is greyed out without having to read source.
#[derive(Debug, PartialEq)]
pub enum StatelessVerdict {
    /// Safe to migrate.
    Stateless,
    /// Migration refused; reason explains which volume kind we found.
    Stateful(String),
}

/// Check whether a compose stack can be migrated without losing data.
///
/// Rule: the stack's `compose_content` must not declare any **named**
/// or **anonymous** volumes. Bind mounts (`/host/path:/in/container`)
/// are tolerated — the operator already accepts that the host path is
/// node-specific, and a Move-to that breaks them is on the operator's
/// head.
///
/// Detection is string-scan rather than YAML-parse for two reasons:
/// (a) the existing codebase already accepts compose_content as
/// opaque YAML strings everywhere else (see `deploy::mod`); pulling
/// in a YAML parser here would be the first place we'd actually
/// validate the structure, raising the bar for malformed input
/// throughout. (b) compose YAML is permissive about formatting; a
/// regex/scan that errs on the side of "looks like a volume → refuse"
/// is the safer default. False positives (refusing a stack that's
/// actually safe) are an inconvenience; false negatives (migrating a
/// stack with state) lose data.
///
/// Heuristic:
/// - Any line starting with `volumes:` outside the `services:` block →
///   top-level named volumes → STATEFUL.
/// - Any `- name:value` entry under a service's `volumes:` block →
///   either a named volume reference or a `:path`-only anonymous
///   volume → STATEFUL.
/// - Bind mounts (`/abs/path:/container/path`) are allowed.
pub fn check_stack_stateless(compose_yaml: &str) -> StatelessVerdict {
    let mut in_service_volumes_block = false;
    let mut current_indent: usize = 0;

    for raw_line in compose_yaml.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
        let stripped = &line[indent..];

        // Reset block markers when we de-indent past them. We can't
        // track precise indent levels without parsing — the rule
        // "any de-indent below a previously-noted block ends it" is
        // close enough.
        if in_service_volumes_block && indent <= current_indent {
            in_service_volumes_block = false;
        }

        // Top-level `volumes:` block (zero indent, exact match) is
        // the canonical compose form for declaring named volumes —
        // if it exists at all, this stack has state.
        if indent == 0 && stripped.starts_with("volumes:") {
            return StatelessVerdict::Stateful(
                "top-level `volumes:` block declares named volumes".into(),
            );
        }

        // A line that looks like a service-level `volumes:` block
        // header. We can't distinguish "the field of a service" from
        // "the top-level field" without indent context, so any indent
        // > 0 starting with `volumes:` is treated as service-level.
        if indent > 0 && stripped.trim_start().starts_with("volumes:") {
            in_service_volumes_block = true;
            current_indent = indent;
            continue;
        }

        if in_service_volumes_block {
            // List entries: `- value` form
            let item = stripped.trim_start();
            if let Some(rest) = item.strip_prefix("- ") {
                let entry = rest.trim();
                // Strip any inline comment.
                let entry = entry.split('#').next().unwrap_or(entry).trim();
                // Bind mounts always start with '/' (linux abs path) or
                // '.' (relative); anything else is a named volume ref
                // or an anonymous volume `:path`.
                let looks_bind = entry.starts_with('/')
                    || entry.starts_with('.')
                    || entry.starts_with("\"/")
                    || entry.starts_with("\".")
                    || entry.starts_with("'/")
                    || entry.starts_with("'.");
                if !looks_bind {
                    return StatelessVerdict::Stateful(format!(
                        "service uses non-bind volume entry: {entry}"
                    ));
                }
            } else if !item.starts_with('-') {
                // exited the list — defensive reset
                in_service_volumes_block = false;
            }
        }
    }

    StatelessVerdict::Stateless
}

/// Same check, indexed off the stack id. Returns AppError for handlers.
pub fn is_stack_stateless(db: &Connection, stack_id: &str) -> AppResult<()> {
    let yaml: Option<String> = db
        .query_row(
            "SELECT compose_content FROM services \
             WHERE id = ?1 AND service_type = 'compose'",
            [stack_id],
            |row| row.get(0),
        )
        .map_err(|_| AppError::NotFound(format!("Stack {stack_id} not found")))?;
    let yaml = yaml.ok_or_else(|| {
        AppError::BadRequest("Stack has no compose content; nothing to migrate".into())
    })?;
    match check_stack_stateless(&yaml) {
        StatelessVerdict::Stateless => Ok(()),
        StatelessVerdict::Stateful(reason) => Err(AppError::BadRequest(format!(
            "{reason}. Stateful migration is on the roadmap — see FUTURE.md."
        ))),
    }
}

/// Atomically set `migration_in_progress = 1` on a stack row, but
/// only if it was previously 0. Returns true when we own the lock,
/// false when another caller beat us to it.
pub fn acquire_migration_lock(db: &Connection, stack_id: &str) -> AppResult<bool> {
    let rows = db.execute(
        "UPDATE services \
         SET migration_in_progress = 1 \
         WHERE id = ?1 \
           AND service_type = 'compose' \
           AND migration_in_progress = 0",
        [stack_id],
    )?;
    Ok(rows == 1)
}

/// Clear the lock unconditionally. Called from the orchestrator's
/// success path and its rollback paths — losing the row mid-flight
/// (operator deleted it before we finished) is fine, no-op.
pub fn release_migration_lock(db: &Connection, stack_id: &str) -> AppResult<()> {
    let _ = db.execute(
        "UPDATE services SET migration_in_progress = 0 WHERE id = ?1",
        [stack_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_is_stateless() {
        assert_eq!(
            check_stack_stateless("services:\n  web:\n    image: nginx"),
            StatelessVerdict::Stateless
        );
    }

    #[test]
    fn top_level_volumes_block_is_stateful() {
        let yaml = "services:\n  web:\n    image: nginx\nvolumes:\n  mydata:\n";
        match check_stack_stateless(yaml) {
            StatelessVerdict::Stateful(reason) => assert!(reason.contains("top-level")),
            other => panic!("expected stateful, got {other:?}"),
        }
    }

    #[test]
    fn service_named_volume_is_stateful() {
        let yaml = "\
services:
  db:
    image: postgres
    volumes:
      - pgdata:/var/lib/postgresql/data
";
        match check_stack_stateless(yaml) {
            StatelessVerdict::Stateful(reason) => assert!(reason.contains("pgdata"), "{reason}"),
            other => panic!("expected stateful, got {other:?}"),
        }
    }

    #[test]
    fn service_anonymous_volume_is_stateful() {
        let yaml = "\
services:
  app:
    image: app
    volumes:
      - :/var/cache
";
        assert!(matches!(
            check_stack_stateless(yaml),
            StatelessVerdict::Stateful(_)
        ));
    }

    #[test]
    fn service_bind_mounts_are_stateless() {
        let yaml = "\
services:
  app:
    image: app
    volumes:
      - /etc/timezone:/etc/timezone:ro
      - ./config:/app/config
";
        assert_eq!(
            check_stack_stateless(yaml),
            StatelessVerdict::Stateless
        );
    }

    #[test]
    fn comments_are_ignored() {
        let yaml = "\
services:
  app:
    image: app
    # volumes:
    #   - mydata:/data
";
        assert_eq!(
            check_stack_stateless(yaml),
            StatelessVerdict::Stateless
        );
    }
}
