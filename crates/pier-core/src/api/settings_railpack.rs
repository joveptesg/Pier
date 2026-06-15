//! Admin endpoints for Railpack auto-build settings.
//!
//! Backs the «Auto-build (Railpack)» tab in the Settings UI. Only the
//! parallel-build cap is exposed today — additional knobs (cache size,
//! BuildKit memory limit) would land here later as new fields.
//!
//! Persistence model: a single row in the `settings` table under the key
//! `railpack.max_parallel_builds`. Read on each process start with
//! priority env > DB > 1 (see [`crate::main`]). The semaphore is created
//! once at boot from the resolved value — UI saves take effect after a
//! `systemctl restart pier`, which the frontend surfaces as a notice.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

const SETTINGS_KEY: &str = "railpack.max_parallel_builds";
const MIN_VAL: u32 = 1;
const MAX_VAL: u32 = 32;

#[derive(Deserialize)]
pub struct UpdateBody {
    pub max_parallel_builds: u32,
}

/// GET /api/v1/admin/settings/railpack
///
/// Returns the saved value, the runtime value (what the live semaphore
/// was created with at boot), and an env-override flag — the UI uses
/// these to render a «restart required» notice when saved ≠ runtime,
/// or a «managed by env var» notice when the override is active.
pub async fn get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let saved = read_saved(&state).unwrap_or(state.railpack_parallel_at_boot as u32);
    let env_override = std::env::var("PIER_RAILPACK_MAX_PARALLEL_BUILDS").is_ok();
    Ok(Json(serde_json::json!({
        "max_parallel_builds": saved,
        "runtime_value": state.railpack_parallel_at_boot,
        "env_override": env_override,
        "min": MIN_VAL,
        "max": MAX_VAL,
    })))
}

/// PUT /api/v1/admin/settings/railpack
pub async fn put(
    State(state): State<SharedState>,
    Json(body): Json<UpdateBody>,
) -> AppResult<impl IntoResponse> {
    if body.max_parallel_builds < MIN_VAL || body.max_parallel_builds > MAX_VAL {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.settings_railpack.max_parallel_builds_range",
            &[("min", &MIN_VAL.to_string()), ("max", &MAX_VAL.to_string())],
        )));
    }
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        rusqlite::params![SETTINGS_KEY, body.max_parallel_builds.to_string()],
    )?;
    tracing::info!(
        "railpack.max_parallel_builds saved as {} (runtime still {} until restart)",
        body.max_parallel_builds,
        state.railpack_parallel_at_boot
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "saved": body.max_parallel_builds,
        "restart_required": (body.max_parallel_builds as usize) != state.railpack_parallel_at_boot,
    })))
}

/// Read the persisted parallel-builds limit. `None` when the row is
/// absent (fresh install) or the value is unparseable. Called from
/// both this module (UI GET) and [`crate::main`] at boot.
pub fn read_saved(state: &SharedState) -> Option<u32> {
    let db = state.db.lock().ok()?;
    let raw: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [SETTINGS_KEY],
            |row| row.get(0),
        )
        .ok()?;
    raw.parse::<u32>()
        .ok()
        .filter(|v| (MIN_VAL..=MAX_VAL).contains(v))
}
