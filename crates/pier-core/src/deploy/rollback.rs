use std::sync::Arc;
use std::time::Instant;

use crate::docker;
use crate::state::AppState;

use super::finish_deployment;

/// Rollback a service to its previous image tag.
pub async fn rollback_service(state: Arc<AppState>, service_id: String) -> Result<String, String> {
    let start = Instant::now();

    // Get service info
    let (name, previous_image, current_image, port) = {
        let db = state.db.lock().map_err(|e| format!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, previous_image_tag, image, port FROM services WHERE id = ?1",
            [&service_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .map_err(|_| format!("Service {service_id} not found"))?
    };

    let previous_image = previous_image
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "No previous image tag available for rollback".to_string())?;

    let stack_name = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    let deploy_id = uuid::Uuid::new_v4().to_string();

    // Create deployment record for rollback
    {
        let db = state.db.lock().map_err(|e| format!("DB lock: {e}"))?;
        let _ = db.execute(
            "INSERT INTO deployments (id, service_id, commit_sha, commit_message, branch, status, triggered_by, image_tag)
             VALUES (?1, ?2, '', 'Rollback to previous version', '', 'building', 'rollback', ?3)",
            rusqlite::params![deploy_id, service_id, previous_image],
        );
        let _ = db.execute(
            "UPDATE services SET status = 'deploying', updated_at = datetime('now') WHERE id = ?1",
            [&service_id],
        );
    }

    let host_port = port.unwrap_or(3000) as u16;
    let container_port: u16 = state
        .db
        .lock()
        .ok()
        .and_then(|db| {
            db.query_row(
                "SELECT container_port FROM port_allocations WHERE service_id = ?1 LIMIT 1",
                [&service_id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .map(|p| p as u16)
        })
        .unwrap_or(3000);

    // Generate compose with previous image
    let yaml = format!(
        "services:\n\
         \x20 app:\n\
         \x20   image: {previous_image}\n\
         \x20   container_name: {stack_name}\n\
         \x20   ports:\n\
         \x20     - \"{host_port}:{container_port}\"\n\
         \x20   restart: unless-stopped\n\
         \x20   labels:\n\
         \x20     pier.service.id: \"{service_id}\"\n"
    );

    // Down existing + deploy with old image
    let _ = docker::compose::down_stack(&stack_name, &state.config).await;
    let mut log = String::new();

    match docker::deploy_service_stack(&state, &service_id, &stack_name, &yaml, None).await {
        Ok(output) => {
            log.push_str(&format!("Rollback deploy: {output}\n"));

            // Swap images: current becomes previous, old becomes current
            if let Ok(db) = state.db.lock() {
                let _ = db.execute(
                    "UPDATE services SET image = ?1, previous_image_tag = ?2, compose_content = ?3, updated_at = datetime('now') WHERE id = ?4",
                    rusqlite::params![previous_image, current_image.unwrap_or_default(), yaml, service_id],
                );
            }

            finish_deployment(&state, &deploy_id, &service_id, "success", &log, start);
            Ok(deploy_id)
        }
        Err(e) => {
            log.push_str(&format!("Rollback failed: {e}\n"));
            finish_deployment(&state, &deploy_id, &service_id, "failed", &log, start);
            Err(format!("Rollback failed: {e}"))
        }
    }
}
