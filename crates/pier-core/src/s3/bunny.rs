use anyhow::Result;
use reqwest::Client;

/// Test Bunny.net storage zone connectivity.
pub async fn test_connection(storage_zone: &str, access_key: &str, endpoint: &str) -> Result<()> {
    let url = format!("https://{endpoint}/{storage_zone}/");
    let client = Client::new();
    let resp = client
        .get(&url)
        .header("AccessKey", access_key)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Bunny.net connection failed: {}", resp.status());
    }
    Ok(())
}

/// Upload file to Bunny.net storage.
pub async fn upload_file(
    storage_zone: &str,
    access_key: &str,
    endpoint: &str,
    path: &str,
    body: Vec<u8>,
) -> Result<()> {
    let url = format!("https://{endpoint}/{storage_zone}/{path}");
    let client = Client::new();
    let resp = client
        .put(&url)
        .header("AccessKey", access_key)
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Bunny upload failed: {}", resp.status());
    }
    Ok(())
}

/// Upload a file to Bunny.net storage straight from disk, without reading it
/// into memory. `size` goes out as `Content-Length` (Bunny needs the length
/// up front; a chunked body is rejected).
pub async fn upload_stream(
    storage_zone: &str,
    access_key: &str,
    endpoint: &str,
    path: &str,
    file: tokio::fs::File,
    size: u64,
) -> Result<()> {
    let url = format!("https://{endpoint}/{storage_zone}/{path}");
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let resp = Client::new()
        .put(&url)
        .header("AccessKey", access_key)
        .header(reqwest::header::CONTENT_LENGTH, size)
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Bunny upload failed: {}", resp.status());
    }
    Ok(())
}

/// Stream an object out of Bunny.net storage without buffering it.
pub async fn download_stream(
    storage_zone: &str,
    access_key: &str,
    endpoint: &str,
    path: &str,
) -> Result<(Option<u64>, axum::body::Body)> {
    let url = format!("https://{endpoint}/{storage_zone}/{path}");
    let resp = Client::new()
        .get(&url)
        .header("AccessKey", access_key)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Bunny download failed: {}", resp.status());
    }
    Ok((
        resp.content_length(),
        axum::body::Body::from_stream(resp.bytes_stream()),
    ))
}

/// Download a single object from Bunny.net storage. Returns the raw body
/// on 200; surfaces a clean error on any non-success status. Used by the
/// embedded npm registry to lazy-rehydrate hot-tier tarballs from cold storage.
pub async fn download_file(
    storage_zone: &str,
    access_key: &str,
    endpoint: &str,
    path: &str,
) -> Result<Vec<u8>> {
    let url = format!("https://{endpoint}/{storage_zone}/{path}");
    let client = Client::new();
    let resp = client
        .get(&url)
        .header("AccessKey", access_key)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Bunny download failed: {}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Delete a single object from Bunny.net storage. 404s are treated as
/// success so cleanup is idempotent (callers may retry without worrying).
pub async fn delete_file(
    storage_zone: &str,
    access_key: &str,
    endpoint: &str,
    path: &str,
) -> Result<()> {
    let url = format!("https://{endpoint}/{storage_zone}/{path}");
    let client = Client::new();
    let resp = client
        .delete(&url)
        .header("AccessKey", access_key)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        return Ok(());
    }
    anyhow::bail!("Bunny delete failed: {status}");
}
