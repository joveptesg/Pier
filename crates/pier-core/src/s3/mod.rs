pub mod bunny;

use std::time::Duration;

use anyhow::{anyhow, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;

/// Build an S3 client with a custom endpoint.
pub fn build_client(
    endpoint: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<Client> {
    let host = endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');

    let creds = Credentials::new(access_key, secret_key, None, None, "pier");
    let region = aws_sdk_s3::config::Region::new(region.to_string());

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(region)
        .endpoint_url(format!("https://{host}"))
        .credentials_provider(creds)
        .force_path_style(true)
        .build();

    Ok(Client::from_conf(config))
}

/// Test S3 connectivity by listing objects (limit 1).
pub async fn test_connection(
    endpoint: &str,
    region: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<()> {
    let client = build_client(endpoint, region, access_key, secret_key)?;
    let fut = client.list_objects_v2().bucket(bucket).max_keys(1).send();
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(res) => {
            res?;
            Ok(())
        }
        Err(_) => Err(anyhow!("timeout: S3 endpoint did not respond within 10s")),
    }
}

/// Upload a file to S3.
pub async fn upload_file(client: &Client, bucket: &str, key: &str, body: Vec<u8>) -> Result<()> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body.into())
        .send()
        .await?;
    Ok(())
}

/// Delete an object from S3.
#[allow(dead_code)]
pub async fn delete_object(client: &Client, bucket: &str, key: &str) -> Result<()> {
    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await?;
    Ok(())
}

/// List objects with a given prefix.
#[allow(dead_code)]
pub async fn list_objects(
    client: &Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<(String, i64)>> {
    let resp = client
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await?;

    let objects = resp
        .contents()
        .iter()
        .map(|obj| {
            (
                obj.key().unwrap_or_default().to_string(),
                obj.size().unwrap_or(0),
            )
        })
        .collect();

    Ok(objects)
}
