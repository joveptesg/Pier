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

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::Connection;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::federation::write_client;
use crate::state::SharedState;

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
///
/// Currently unused — the orchestrator inlines the equivalent check
/// inside its source-validation step so it can pull `compose_content`
/// and the statelessness verdict from the same query. Kept as a
/// public helper because the UI eligibility check (Этап 4.3) will
/// reach for it.
#[allow(dead_code)]
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

#[derive(Deserialize)]
pub struct MigrateRequest {
    /// `servers.id` of the destination peer. Must be paired for
    /// federation (servers.federation_token IS NOT NULL) or the
    /// orchestrator can't talk to it.
    pub target_server_id: String,
}

/// POST /api/v1/stacks/{id}/migrate
///
/// Move a locally-owned compose stack to a federated peer. Pipeline:
///
/// 1. Validate: source stack exists locally, is owner_server_id NULL
///    (locally managed), is stateless per [`check_stack_stateless`].
/// 2. Acquire migration lock (atomic UPDATE on services row).
/// 3. Resolve target peer through [`write_client::lookup_write_peer`].
///    Fails fast with 400 if not paired.
/// 4. Snapshot YAML.
/// 5. Create stack on target via `write_client::create_stack`.
/// 6. Deploy stack on target via `write_client::deploy_stack`.
/// 7. Tear down on source via `docker::compose::down_stack` +
///    `docker::compose::remove_stack`.
/// 8. Delete source row from `services`, releasing the lock with it.
///
/// Failure handling per step is documented inline. Domain cut-over
/// is NOT automated — each Pier node has its own Traefik instance,
/// so an external DNS A-record change is still on the operator's
/// plate. The response includes a `domain_advice` field listing the
/// domains the operator should re-point, since Pier knows them and
/// the alternative is making the operator hunt them down by hand.
pub async fn migrate_stack(
    State(state): State<SharedState>,
    Path(stack_id): Path<String>,
    Json(body): Json<MigrateRequest>,
) -> AppResult<impl IntoResponse> {
    // --- Step 1: validate source -----------------------------------
    let (source_name, source_yaml, source_domains) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let row = db
            .query_row(
                "SELECT name, compose_content, owner_server_id \
                 FROM services WHERE id = ?1 AND service_type = 'compose'",
                [&stack_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(|_| AppError::NotFound(format!("Stack {stack_id} not found")))?;
        let (name, yaml, owner) = row;
        if owner.is_some() {
            return Err(AppError::BadRequest(
                "Stack is managed by a remote primary; migrate from there instead".into(),
            ));
        }
        let yaml = yaml.ok_or_else(|| {
            AppError::BadRequest("Stack has no compose content; nothing to migrate".into())
        })?;
        match check_stack_stateless(&yaml) {
            StatelessVerdict::Stateless => {}
            StatelessVerdict::Stateful(reason) => {
                return Err(AppError::BadRequest(format!(
                    "{reason}. Stateful migration is on the roadmap — see FUTURE.md."
                )));
            }
        }
        // Best-effort domain enumeration. The `domains` table can also
        // carry rows for compose stacks (via service_id), but stacks
        // typically declare routing inline as Traefik labels. We
        // return whatever's there so the operator's DNS check is
        // complete.
        let mut stmt = db.prepare(
            "SELECT domain FROM domains WHERE service_id = ?1 ORDER BY domain",
        )?;
        let domains: Vec<String> = stmt
            .query_map([&stack_id], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        (name, yaml, domains)
    };

    // --- Step 2: acquire lock --------------------------------------
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if !acquire_migration_lock(&db, &stack_id)? {
            return Err(AppError::Conflict(
                "Another migration is already in progress for this stack".into(),
            ));
        }
    }

    // From here on, any early return MUST release the lock. We wrap
    // the rest in a helper closure pattern + an explicit release on
    // every exit path.
    let result = run_migration_pipeline(
        &state,
        &stack_id,
        &source_name,
        &source_yaml,
        &body.target_server_id,
        &source_domains,
    )
    .await;

    // --- Final lock release on the failure path ---------------------
    // On the success path the source row is gone (step 8), which drops
    // the lock with it. On failure the row is still around and we
    // need to clear the flag explicitly so a retry can take.
    if result.is_err() {
        if let Ok(db) = state.db.lock() {
            let _ = release_migration_lock(&db, &stack_id);
        }
    }
    result
}

async fn run_migration_pipeline(
    state: &SharedState,
    stack_id: &str,
    source_name: &str,
    source_yaml: &str,
    target_server_id: &str,
    source_domains: &[String],
) -> AppResult<axum::response::Response> {
    // --- Step 3: resolve target peer ------------------------------
    let peer = write_client::lookup_write_peer(state, target_server_id)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "Target {target_server_id} is not paired for federation; set its token in /servers/<id>"
            ))
        })?;

    // --- Step 4+5: create + deploy on target ----------------------
    // create_stack returns {"ok": true, "id": "<uuid>"} from peer.
    let create_resp = write_client::create_stack(&peer, source_name, source_yaml)
        .await
        .map_err(|e| AppError::BadRequest(format!("target rejected create: {e:#}")))?;
    let new_stack_id = create_resp
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "target {} returned no stack id from create",
                peer.name
            ))
        })?
        .to_string();

    if let Err(e) = write_client::deploy_stack(&peer, &new_stack_id).await {
        // Target has the row but containers didn't come up. Try to
        // clean up the target row so a retry doesn't end up with
        // ghost stacks accumulating. Best-effort; surface the
        // original error if cleanup also fails.
        let _ = write_client::delete_stack(&peer, &new_stack_id).await;
        return Err(AppError::BadRequest(format!(
            "target deploy failed (target row rolled back): {e:#}"
        )));
    }

    // --- Step 7+8: tear down + remove source ----------------------
    // From here on the operator's traffic should be going to the
    // target. Source teardown failures are logged but don't fail the
    // request — leaving a stopped container around is recoverable;
    // returning an error to the UI would hide the fact that the
    // target is live.
    let mut teardown_warning: Option<String> = None;
    if let Err(e) = crate::docker::compose::down_stack(source_name, &state.config).await {
        tracing::warn!("migrate: source down failed for {source_name}: {e}");
        teardown_warning = Some(format!("source down warning: {e}"));
    }
    if let Err(e) = crate::docker::compose::remove_stack(source_name, &state.config).await {
        tracing::warn!("migrate: source remove failed for {source_name}: {e}");
        teardown_warning = Some(format!(
            "{}{}source remove warning: {e}",
            teardown_warning.clone().unwrap_or_default(),
            if teardown_warning.is_some() { "; " } else { "" }
        ));
    }
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute("DELETE FROM services WHERE id = ?1", [stack_id])?;
    }

    // Kick a federation_sync so the dashboard shows the new
    // federated card immediately instead of waiting up to 45s.
    let _ = crate::federation::sync::run_sync_pass(state).await;

    Ok(Json(serde_json::json!({
        "ok": true,
        "moved_to": peer.name,
        "new_stack_id": new_stack_id,
        "domain_advice": source_domains,
        "teardown_warning": teardown_warning,
    }))
    .into_response())
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
