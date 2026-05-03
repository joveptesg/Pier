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
