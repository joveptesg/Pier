//! Platform-side repair for Postgres services whose cluster sits in an
//! anonymous Docker volume.
//!
//! See [`crate::docker::pgdata_migration`] for what the broken layout is and
//! why it loses data. This module is the acting half:
//!
//! - `GET  /api/v1/resources/{id}/pgdata-status` — report the diagnosis so the
//!   UI can surface a warning on affected services.
//! - `POST /api/v1/resources/{id}/pgdata-repair` — stop the service, copy the
//!   cluster into the named volume, pin `PGDATA` there, redeploy.
//!
//! The repair never deletes the original volume. On success the old anonymous
//! volume is still on disk untouched, so a bad outcome is recoverable by
//! removing the `PGDATA` env var and redeploying.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountTypeEnum};
use bollard::query_parameters::{
    CreateContainerOptions, ListContainersOptions, RemoveContainerOptions, WaitContainerOptions,
};
use futures_util::StreamExt;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::docker::pgdata_migration::{
    diagnose, is_affected_catalog, needs_env_pin, plan_repair, PgdataDiagnosis, RepairPlan,
};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// Mount points used inside the throwaway copy container.
const SRC_MOUNT: &str = "/pier-src";
const DST_MOUNT: &str = "/pier-dst";

/// Exit code the copy script uses when the destination already holds files.
const EXIT_TARGET_NOT_EMPTY: i64 = 3;

/// Locate the live container for `service_id`: by the `pier.service.id` label
/// first, falling back to `services.container_id`. Mirrors the lookup in
/// [`crate::docker::recreate`] so both agree on which container a service is.
async fn find_container(state: &SharedState, service_id: &str) -> AppResult<String> {
    let list = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("list containers: {e}")))?;

    let by_label = list
        .iter()
        .find(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get("pier.service.id"))
                .is_some_and(|s| s == service_id)
        })
        .and_then(|c| c.id.clone());
    if let Some(id) = by_label {
        return Ok(id);
    }

    let cid: Option<String> = {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.query_row(
            "SELECT container_id FROM services WHERE id = ?1",
            [service_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    };
    cid.filter(|c| !c.is_empty())
        .ok_or_else(|| AppError::NotFound(format!("no running container for service {service_id}")))
}

/// Whether this service is a Postgres-family catalog service. Anything else is
/// out of scope: the anonymous-volume trap here is specific to how the Postgres
/// images move `PGDATA` between major versions, and relocating some other
/// image's data directory on that reasoning would be guesswork.
fn is_postgres_service(state: &SharedState, service_id: &str) -> bool {
    let Ok(db) = state.db.lock() else {
        return false;
    };
    db.query_row(
        "SELECT catalog_id FROM services WHERE id = ?1",
        [service_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .is_some_and(|c| is_affected_catalog(&c))
}

/// Inspect the container and its image, and work out where the cluster lives.
/// Returns the diagnosis alongside the container id and image reference.
async fn inspect_and_diagnose(
    state: &SharedState,
    service_id: &str,
) -> AppResult<(String, String, PgdataDiagnosis)> {
    let container_id = find_container(state, service_id).await?;
    let info = state
        .docker
        .inspect_container(&container_id, None)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("inspect container: {e}")))?;

    let container_env = info
        .config
        .as_ref()
        .and_then(|c| c.env.clone())
        .unwrap_or_default();
    let image = info
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .or_else(|| info.image.clone())
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("container has no image reference")))?;

    // The image's own PGDATA matters when the container doesn't set one — that
    // is the common case for services created before this repair existed.
    let image_env = state
        .docker
        .inspect_image(&image)
        .await
        .ok()
        .and_then(|i| i.config.and_then(|c| c.env))
        .unwrap_or_default();

    let mounts = info.mounts.clone().unwrap_or_default();
    let d = diagnose(&container_env, &image_env, &mounts);
    Ok((container_id, image, d))
}

/// Decrypt the service's stored env vars.
fn service_env(state: &SharedState, id: &str) -> AppResult<HashMap<String, String>> {
    let db = state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
    let stored: Option<String> = db
        .query_row("SELECT env_json FROM services WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .map_err(|_| AppError::NotFound(format!("resource {id} not found")))?;
    Ok(
        serde_json::from_str(&crate::crypto::decrypt_env_json(stored.as_deref()))
            .unwrap_or_default(),
    )
}

fn diagnosis_json(
    d: &PgdataDiagnosis,
    plan: Option<&RepairPlan>,
    env_pin_needed: bool,
) -> serde_json::Value {
    serde_json::json!({
        "pgdata": d.pgdata,
        "backing_volume": d.backing_volume,
        "at_risk": d.backing_is_anonymous,
        // Data is already on a named volume but the path isn't recorded in the
        // service env, so the next compose regeneration would lose it.
        "needs_env_pin": env_pin_needed,
        "needs_attention": d.backing_is_anonymous || env_pin_needed,
        "repairable": plan.is_some() || env_pin_needed,
        "named_volume": d.named_volume.as_ref().map(|n| &n.name),
        "target_pgdata": plan.map(|p| p.target_pgdata.clone()).unwrap_or_else(|| d.pgdata.clone()),
    })
}

/// GET /api/v1/resources/{id}/pgdata-status
///
/// `at_risk` means the cluster is in an anonymous volume. `repairable` means
/// the automatic fix can run — false with `at_risk` true indicates a service
/// with no named volume to move the data into, which needs an operator.
pub async fn status(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;

    if !is_postgres_service(&state, &id) {
        return Ok(Json(serde_json::json!({
            "at_risk": false,
            "needs_attention": false,
            "repairable": false,
            "reason": "not a PostgreSQL-family service",
        })));
    }

    // A service with no container yet simply isn't at risk — report that
    // rather than erroring, so the UI can call this unconditionally.
    let Ok((_, _, d)) = inspect_and_diagnose(&state, &id).await else {
        return Ok(Json(serde_json::json!({
            "at_risk": false,
            "needs_attention": false,
            "repairable": false,
            "reason": "no container to inspect",
        })));
    };
    let plan = plan_repair(&d);
    let env = service_env(&state, &id).unwrap_or_default();
    let pin = needs_env_pin(&d, env.get("PGDATA").map(String::as_str));
    Ok(Json(diagnosis_json(&d, plan.as_ref(), pin)))
}

/// Run `cmd` in a throwaway container built from `image`, with the two volumes
/// attached read-only / read-write. Returns the exit code, and the container's
/// output when it failed.
async fn run_copy_container(
    state: &SharedState,
    image: &str,
    plan: &RepairPlan,
    script: &str,
) -> AppResult<(i64, String)> {
    let body = ContainerCreateBody {
        image: Some(image.to_string()),
        entrypoint: Some(vec!["/bin/sh".to_string()]),
        cmd: Some(vec!["-c".to_string(), script.to_string()]),
        // Run as root: the cluster files are owned by the postgres uid and
        // `cp -a` must preserve that ownership verbatim.
        user: Some("0:0".to_string()),
        host_config: Some(HostConfig {
            mounts: Some(vec![
                Mount {
                    source: Some(plan.source_volume.clone()),
                    target: Some(SRC_MOUNT.to_string()),
                    typ: Some(MountTypeEnum::VOLUME),
                    read_only: Some(true),
                    ..Default::default()
                },
                Mount {
                    source: Some(plan.target_volume.clone()),
                    target: Some(DST_MOUNT.to_string()),
                    typ: Some(MountTypeEnum::VOLUME),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let created = state
        .docker
        .create_container(None::<CreateContainerOptions>, body)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("create copy container: {e}")))?;
    let cid = created.id;

    let result = async {
        crate::docker::containers::start_container(&state.docker, &cid)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("start copy container: {e}")))?;

        let mut wait = state
            .docker
            .wait_container(&cid, None::<WaitContainerOptions>);
        let mut code = -1;
        while let Some(msg) = wait.next().await {
            match msg {
                Ok(r) => code = r.status_code,
                // Docker reports a non-zero exit as a stream error carrying the
                // same status code; treat it as the exit code, not a transport
                // failure, so the caller can act on EXIT_TARGET_NOT_EMPTY.
                Err(bollard::errors::Error::DockerContainerWaitError { code: c, .. }) => code = c,
                Err(e) => {
                    return Err(AppError::Internal(anyhow::anyhow!("wait copy: {e}")));
                }
            }
        }

        let logs = crate::docker::logs::get_logs(&state.docker, &cid, 50, false)
            .await
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        Ok((code, logs))
    }
    .await;

    // Always clean up the helper. `v: false` — its volumes are the service's
    // real data, they must outlive this container.
    let _ = state
        .docker
        .remove_container(
            &cid,
            Some(RemoveContainerOptions {
                force: true,
                v: false,
                ..Default::default()
            }),
        )
        .await;

    result
}

/// POST /api/v1/resources/{id}/pgdata-repair
///
/// Stops the service, copies `PGDATA` from the anonymous volume into the named
/// one, records `PGDATA` in the service env, and redeploys through the normal
/// env-update path. Idempotent: a healthy service returns `changed: false`
/// without touching anything.
pub async fn repair(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    // Admin-level: this stops a database and moves its files.
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;

    if !is_postgres_service(&state, &id) {
        return Err(AppError::BadRequest(format!(
            "resource {id} is not a PostgreSQL-family service"
        )));
    }

    let (container_id, image, d) = inspect_and_diagnose(&state, &id).await?;
    let mut env = service_env(&state, &id)?;
    let pin = needs_env_pin(&d, env.get("PGDATA").map(String::as_str));

    let Some(plan) = plan_repair(&d) else {
        // Data is already safe; only the service env is out of step with the
        // running container. Record the path and redeploy — no stop, no copy.
        if pin {
            tracing::info!(
                "pgdata-repair: service {id} already stores its cluster on a named volume; \
                 recording PGDATA={} in the service env so compose regeneration keeps it",
                d.pgdata
            );
            env.insert("PGDATA".to_string(), d.pgdata.clone());
            crate::api::env::apply_env_update(&state, &id, env, true).await?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "changed": true,
                "pgdata": d.pgdata,
                "note": "PGDATA recorded in the service environment; data was already in the named volume",
            })));
        }
        return Ok(Json(serde_json::json!({
            "ok": true,
            "changed": false,
            "reason": if d.backing_is_anonymous {
                "cluster is in an anonymous volume but the service has no named volume to move it into"
            } else {
                "cluster already lives in a named volume"
            },
            "diagnosis": diagnosis_json(&d, None, false),
        })));
    };

    tracing::warn!(
        "pgdata-repair: service {id} cluster at {} is backed by anonymous volume {} — moving into {}:{}",
        d.pgdata,
        plan.source_volume,
        plan.target_volume,
        plan.target_path_in_volume
    );

    // Quiesce first: copying a running cluster would capture a torn state.
    crate::docker::containers::stop_container(&state.docker, &container_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("stop service: {e}")))?;

    // Refuse to write into a non-empty destination — that would mean a previous
    // attempt left files behind, and silently merging into them risks a corrupt
    // cluster. `cp -a` keeps uid/gid/permissions, which Postgres requires.
    let script = format!(
        "set -e\n\
         mkdir -p {dst}{tgt}\n\
         if [ -n \"$(ls -A {dst}{tgt} 2>/dev/null)\" ]; then echo 'destination not empty'; exit {code}; fi\n\
         cp -a {src}/. {dst}{tgt}/\n\
         test -f {dst}{tgt}/PG_VERSION\n",
        src = SRC_MOUNT,
        dst = DST_MOUNT,
        tgt = plan.target_path_in_volume,
        code = EXIT_TARGET_NOT_EMPTY,
    );

    let copy = run_copy_container(&state, &image, &plan, &script).await;

    // Any failure before the env change: put the service back up as it was.
    let (code, logs) = match copy {
        Ok(v) => v,
        Err(e) => {
            let _ = crate::docker::containers::start_container(&state.docker, &container_id).await;
            return Err(e);
        }
    };
    if code != 0 {
        let _ = crate::docker::containers::start_container(&state.docker, &container_id).await;
        let hint = if code == EXIT_TARGET_NOT_EMPTY {
            format!(
                " — {} already contains files in volume {}; inspect it before retrying",
                plan.target_path_in_volume, plan.target_volume
            )
        } else {
            String::new()
        };
        return Err(AppError::Internal(anyhow::anyhow!(
            "copying PGDATA failed (exit {code}){hint}: {logs}"
        )));
    }

    tracing::info!(
        "pgdata-repair: service {id} cluster copied to {}:{}; pinning PGDATA={}",
        plan.target_volume,
        plan.target_path_in_volume,
        plan.target_pgdata
    );

    // Pin PGDATA and redeploy through the same path a "Save & Redeploy" uses,
    // so compose regeneration stays in exactly one place.
    env.insert("PGDATA".to_string(), plan.target_pgdata.clone());

    if let Err(e) = crate::api::env::apply_env_update(&state, &id, env, true).await {
        // The data is copied and intact in both volumes; only the redeploy
        // failed. Bring the old container back so the service keeps serving
        // from its original location while the operator investigates.
        let _ = crate::docker::containers::start_container(&state.docker, &container_id).await;
        return Err(e);
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "changed": true,
        "moved_from": plan.source_volume,
        "moved_to": format!("{}:{}", plan.target_volume, plan.target_path_in_volume),
        "pgdata": plan.target_pgdata,
        "note": "the original volume was left untouched and can be removed once the service is verified",
    })))
}
