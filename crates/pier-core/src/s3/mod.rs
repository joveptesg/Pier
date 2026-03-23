pub mod bunny;

use anyhow::Result;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;

/// Build an S3 client with a custom endpoint.
pub fn build_client(
    endpoint: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<Client> {
    let creds = Credentials::new(access_key, secret_key, None, None, "pier");
    let region = aws_sdk_s3::config::Region::new(region.to_string());

    let config = aws_sdk_s3::Config::builder()
        .region(region)
        .endpoint_url(format!("https://{endpoint}"))
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
    client
        .list_objects_v2()
        .bucket(bucket)
        .max_keys(1)
        .send()
        .await?;
    Ok(())
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
