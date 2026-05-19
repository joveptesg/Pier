//! Mesh service-DNS CRUD — backs migration 54.
//!
//! Operators register **logical** names (`db`, `cache`, `auth`) that
//! resolve to whichever node currently hosts the service. The deploy
//! pipeline picks these up via [`crate::deploy::mod`] and injects
//! matching `extra_hosts` entries into every stack on deploy.
//!
//! Routes (all behind the standard session auth — these are operator
//! actions, not federation):
//! - `GET    /api/v1/network/service-dns`           — list everything.
//! - `POST   /api/v1/network/service-dns`           — add a mapping.
//! - `PUT    /api/v1/network/service-dns/{name}`    — repoint to a
//!   different server / service / port.
//! - `DELETE /api/v1/network/service-dns/{name}`    — drop a mapping.
//!
//! Validation rules in [`validate_name`]:
//! - lowercase ASCII `[a-z]([a-z0-9-]{0,30}[a-z0-9])?`
//! - cannot collide with an existing server name (a `vps1.mesh` host
//!   already exists from the server-mesh injection at Etap 0.3f).
//!
//! We deliberately do NOT trigger redeploys here. That responsibility
//! lives in the next phase (3.4) so this CRUD remains side-effect-free
//! and unit-testable.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateMappingRequest {
    pub name: String,
    pub server_id: String,
    #[serde(default)]
    pub service_id: Option<String>,
    pub port: i64,
}

#[derive(Deserialize)]
pub struct UpdateMappingRequest {
    pub server_id: String,
    #[serde(default)]
    pub service_id: Option<String>,
    pub port: i64,
}

/// GET /api/v1/network/service-dns
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT sd.name, sd.server_id, s.name AS server_name, sd.service_id, sd.port, \
                sd.created_at, sd.updated_at \
         FROM service_dns sd \
         LEFT JOIN servers s ON s.id = sd.server_id \
         ORDER BY sd.name",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "name": row.get::<_, String>(0)?,
                "server_id": row.get::<_, String>(1)?,
                "server_name": row.get::<_, Option<String>>(2)?,
                "service_id": row.get::<_, Option<String>>(3)?,
                "port": row.get::<_, i64>(4)?,
                "created_at": row.get::<_, i64>(5)?,
                "updated_at": row.get::<_, i64>(6)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

/// POST /api/v1/network/service-dns
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateMappingRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_ascii_lowercase();
    validate_name(&name)?;
    validate_port(body.port)?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Refuse collisions with existing server names — those already
    // resolve to `<server>.mesh` via the Etap 0.3f injection, and
    // having two extra_hosts entries with the same hostname is
    // undefined behaviour (compose picks one, you don't get to say
    // which).
    let server_name_exists: bool = db
        .query_row(
            "SELECT 1 FROM servers WHERE LOWER(name) = ?1",
            [&name],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if server_name_exists {
        return Err(AppError::Conflict(format!(
            "name '{name}' already refers to a server ({name}.mesh); pick a different label"
        )));
    }

    let now = chrono::Utc::now().timestamp();
    let result = db.execute(
        "INSERT INTO service_dns (name, server_id, service_id, port, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        rusqlite::params![name, body.server_id, body.service_id, body.port, now],
    );
    match result {
        Ok(_) => {
            drop(db);
            // Existing containers cache extra_hosts at create-time, so
            // a fresh row only reaches them after a redeploy. Kick that
            // in the background so the API call stays fast.
            crate::deploy::spawn_redeploy_all_compose(state.clone());
            Ok(Json(
                serde_json::json!({"ok": true, "name": name, "redeploy": "queued"}),
            ))
        }
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            // FK on server_id, or PK collision on name. The user-facing
            // diff matters, so disambiguate by re-querying.
            let name_taken: bool = db
                .query_row("SELECT 1 FROM service_dns WHERE name = ?1", [&name], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
                .is_some();
            if name_taken {
                Err(AppError::Conflict(format!(
                    "name '{name}' is already registered"
                )))
            } else {
                Err(AppError::BadRequest(format!(
                    "server_id '{}' does not exist",
                    body.server_id
                )))
            }
        }
        Err(e) => Err(AppError::Internal(anyhow::anyhow!(e))),
    }
}

/// PUT /api/v1/network/service-dns/{name}
pub async fn update(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateMappingRequest>,
) -> AppResult<impl IntoResponse> {
    let name = name.trim().to_ascii_lowercase();
    validate_name(&name)?;
    validate_port(body.port)?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute(
        "UPDATE service_dns \
         SET server_id = ?2, service_id = ?3, port = ?4, updated_at = ?5 \
         WHERE name = ?1",
        rusqlite::params![
            name,
            body.server_id,
            body.service_id,
            body.port,
            chrono::Utc::now().timestamp(),
        ],
    )?;
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "service-DNS mapping '{name}' not found"
        )));
    }
    drop(db);
    crate::deploy::spawn_redeploy_all_compose(state.clone());
    Ok(Json(serde_json::json!({
        "ok": true,
        "name": name,
        "redeploy": "queued",
    })))
}

/// DELETE /api/v1/network/service-dns/{name}
pub async fn remove(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = name.trim().to_ascii_lowercase();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM service_dns WHERE name = ?1", [&name])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "service-DNS mapping '{name}' not found"
        )));
    }
    drop(db);
    crate::deploy::spawn_redeploy_all_compose(state.clone());
    Ok(Json(serde_json::json!({
        "ok": true,
        "action": "deleted",
        "redeploy": "queued",
    })))
}

/// Validate a hostname leaf. Mirrors RFC 1123 label rules tightened to
/// lowercase-only: leading alphanumeric, internal `[a-z0-9-]`, trailing
/// alphanumeric, total length 1..=31. The 31-char cap leaves headroom
/// for the `.mesh` suffix without bumping into the 63-char DNS label
/// limit when combined with any operator-prefixing in future versions.
pub fn validate_name(name: &str) -> AppResult<()> {
    if name.is_empty() || name.len() > 31 {
        return Err(AppError::BadRequest(
            "name must be 1..=31 characters".into(),
        ));
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(AppError::BadRequest("name must start with [a-z0-9]".into()));
    }
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return Err(AppError::BadRequest("name must end with [a-z0-9]".into()));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err(AppError::BadRequest(
            "name may only contain lowercase letters, digits, and '-'".into(),
        ));
    }
    Ok(())
}

fn validate_port(port: i64) -> AppResult<()> {
    if !(1..=65535).contains(&port) {
        return Err(AppError::BadRequest("port must be in 1..=65535".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_simple_labels() {
        for n in &["db", "cache", "auth", "db-1", "x", "0"] {
            assert!(validate_name(n).is_ok(), "rejected {n}");
        }
    }

    #[test]
    fn validate_name_rejects_garbage() {
        let oversized = "a".repeat(32);
        let bad = [
            "",
            "-db",
            "db-",
            "Db",
            "DB",
            "_db",
            "db.cache",
            "  ",
            oversized.as_str(),
        ];
        for n in bad {
            assert!(validate_name(n).is_err(), "accepted {n:?}");
        }
    }

    #[test]
    fn validate_port_range() {
        assert!(validate_port(1).is_ok());
        assert!(validate_port(5432).is_ok());
        assert!(validate_port(65535).is_ok());
        assert!(validate_port(0).is_err());
        assert!(validate_port(65536).is_err());
        assert!(validate_port(-1).is_err());
    }
}
