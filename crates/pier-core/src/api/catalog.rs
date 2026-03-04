use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CatalogQuery {
    pub category: Option<String>,
    pub search: Option<String>,
}

/// GET /api/v1/catalog — list all catalog templates.
pub async fn list(
    State(state): State<SharedState>,
    Query(query): Query<CatalogQuery>,
) -> AppResult<impl IntoResponse> {
    let items: Vec<serde_json::Value> = state
        .catalog
        .iter()
        .filter(|item| {
            if let Some(cat) = &query.category {
                if item.meta.category != *cat {
                    return false;
                }
            }
            if let Some(search) = &query.search {
                let s = search.to_lowercase();
                if !item.meta.name.to_lowercase().contains(&s)
                    && !item.meta.description.to_lowercase().contains(&s)
                    && !item.meta.tags.iter().any(|t| t.to_lowercase().contains(&s))
                {
                    return false;
                }
            }
            true
        })
        .map(|item| {
            serde_json::json!({
                "id": item.meta.id,
                "name": item.meta.name,
                "description": item.meta.description,
                "category": item.meta.category,
                "icon": item.meta.icon,
                "tags": item.meta.tags,
            })
        })
        .collect();

    Ok(Json(items))
}

/// GET /api/v1/catalog/{id} — get full details of a catalog template.
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let item = state
        .catalog
        .iter()
        .find(|i| i.meta.id == id)
        .ok_or_else(|| AppError::NotFound(format!("Catalog template '{id}' not found")))?;

    // Build fields list with resolved options
    let fields: Vec<serde_json::Value> = if let Some(ui) = &item.ui {
        ui.fields
            .iter()
            .map(|(key, field)| {
                let mut f = serde_json::json!({
                    "key": key,
                    "type": field.field_type,
                    "label": field.label,
                    "required": field.required,
                    "auto_generate": field.auto_generate,
                });

                if let Some(d) = &field.default {
                    f["default"] = serde_json::Value::String(d.clone());
                }
                if let Some(p) = &field.placeholder {
                    f["placeholder"] = serde_json::Value::String(p.clone());
                }
                if let Some(m) = &field.maps_to {
                    f["maps_to"] = serde_json::Value::String(m.clone());
                }

                // Resolve options from versions
                if let Some(opts_from) = &field.options_from {
                    if opts_from == "versions.available" {
                        if let Some(versions) = &item.versions {
                            f["options"] = serde_json::json!(versions.available);
                            f["default"] = serde_json::Value::String(versions.default.clone());
                        }
                    }
                }
                if let Some(opts) = &field.options {
                    f["options"] = serde_json::json!(opts);
                }

                f
            })
            .collect()
    } else {
        Vec::new()
    };

    let ports: Vec<serde_json::Value> = item
        .ports
        .iter()
        .map(|(name, p)| {
            serde_json::json!({
                "name": name,
                "internal": p.internal,
                "protocol": p.protocol,
                "description": p.description,
            })
        })
        .collect();

    let mut result = serde_json::json!({
        "id": item.meta.id,
        "name": item.meta.name,
        "description": item.meta.description,
        "category": item.meta.category,
        "icon": item.meta.icon,
        "tags": item.meta.tags,
        "fields": fields,
        "ports": ports,
        "has_compose_template": item.compose.is_some(),
    });

    if let Some(cluster) = &item.cluster {
        result["cluster"] = serde_json::json!({
            "supported": cluster.supported,
            "min_nodes": cluster.min_nodes,
            "max_nodes": cluster.max_nodes,
            "default_nodes": cluster.default_nodes,
            "description": cluster.description,
        });
    }

    Ok(Json(result))
}
