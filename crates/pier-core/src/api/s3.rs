use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateS3Request {
    pub name: String,
    #[serde(default = "default_s3_type")]
    pub storage_type: String,
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default)]
    pub key_prefix: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateS3Request {
    pub name: String,
    #[serde(default = "default_s3_type")]
    pub storage_type: String,
    pub endpoint: String,
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub key_prefix: Option<String>,
}

fn default_s3_type() -> String {
    "s3".to_string()
}

/// Strip leading/trailing slashes and whitespace; the join in build_s3_key
/// adds the separator itself, so the stored value never includes them.
fn normalize_prefix(p: &str) -> String {
    p.trim().trim_matches('/').to_string()
}

/// GET /api/v1/s3
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, storage_type, endpoint, region, bucket, access_key, key_prefix, created_at
         FROM s3_storages WHERE is_active = 1 ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "storage_type": row.get::<_, String>(2)?,
                "endpoint": row.get::<_, String>(3)?,
                "region": row.get::<_, String>(4)?,
                "bucket": row.get::<_, String>(5)?,
                "access_key": row.get::<_, String>(6)?,
                "key_prefix": row.get::<_, String>(7)?,
                "created_at": row.get::<_, String>(8)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/s3
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateS3Request>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty()
        || body.endpoint.trim().is_empty()
        || body.bucket.trim().is_empty()
    {
        return Err(AppError::BadRequest(
            "Name, endpoint, and bucket are required".into(),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let key_prefix = body
        .key_prefix
        .as_deref()
        .map(normalize_prefix)
        .unwrap_or_else(|| "pier-backups".to_string());
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO s3_storages (id, name, storage_type, endpoint, region, bucket, access_key, secret_key, key_prefix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.storage_type,
            body.endpoint.trim(),
            body.region,
            body.bucket.trim(),
            body.access_key,
            body.secret_key,
            key_prefix,
        ],
    )?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// PUT /api/v1/s3/{id}
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateS3Request>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty()
        || body.endpoint.trim().is_empty()
        || body.bucket.trim().is_empty()
    {
        return Err(AppError::BadRequest(
            "Name, endpoint, and bucket are required".into(),
        ));
    }
    let key_prefix = body.key_prefix.as_deref().map(normalize_prefix);
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = match (body.secret_key.is_empty(), key_prefix) {
        (true, None) => db.execute(
            "UPDATE s3_storages
             SET name = ?1, storage_type = ?2, endpoint = ?3, region = ?4,
                 bucket = ?5, access_key = ?6
             WHERE id = ?7",
            rusqlite::params![
                body.name.trim(),
                body.storage_type,
                body.endpoint.trim(),
                body.region,
                body.bucket.trim(),
                body.access_key,
                id,
            ],
        )?,
        (true, Some(prefix)) => db.execute(
            "UPDATE s3_storages
             SET name = ?1, storage_type = ?2, endpoint = ?3, region = ?4,
                 bucket = ?5, access_key = ?6, key_prefix = ?7
             WHERE id = ?8",
            rusqlite::params![
                body.name.trim(),
                body.storage_type,
                body.endpoint.trim(),
                body.region,
                body.bucket.trim(),
                body.access_key,
                prefix,
                id,
            ],
        )?,
        (false, None) => db.execute(
            "UPDATE s3_storages
             SET name = ?1, storage_type = ?2, endpoint = ?3, region = ?4,
                 bucket = ?5, access_key = ?6, secret_key = ?7
             WHERE id = ?8",
            rusqlite::params![
                body.name.trim(),
                body.storage_type,
                body.endpoint.trim(),
                body.region,
                body.bucket.trim(),
                body.access_key,
                body.secret_key,
                id,
            ],
        )?,
        (false, Some(prefix)) => db.execute(
            "UPDATE s3_storages
             SET name = ?1, storage_type = ?2, endpoint = ?3, region = ?4,
                 bucket = ?5, access_key = ?6, secret_key = ?7, key_prefix = ?8
             WHERE id = ?9",
            rusqlite::params![
                body.name.trim(),
                body.storage_type,
                body.endpoint.trim(),
                body.region,
                body.bucket.trim(),
                body.access_key,
                body.secret_key,
                prefix,
                id,
            ],
        )?,
    };
    if rows == 0 {
        return Err(AppError::NotFound(format!("S3 storage {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/s3/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM s3_storages WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("S3 storage {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/s3/{id}/test
pub async fn test(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key
             FROM s3_storages WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("S3 storage {id} not found")))?
    };

    match storage_type.as_str() {
        "bunny" => {
            crate::s3::bunny::test_connection(&bucket, &access_key, &endpoint)
                .await
                .map_err(|e| AppError::BadRequest(format!("Bunny.net test failed: {e}")))?;
        }
        _ => {
            crate::s3::test_connection(&endpoint, &region, &bucket, &access_key, &secret_key)
                .await
                .map_err(|e| AppError::BadRequest(format!("S3 test failed: {e}")))?;
        }
    }

    Ok(Json(
        serde_json::json!({"ok": true, "message": "Connection successful"}),
    ))
}
