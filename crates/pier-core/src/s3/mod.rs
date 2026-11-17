pub mod bunny;

use std::time::Duration;

use anyhow::{anyhow, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;

/// Build an S3 client with a custom endpoint.
///
/// `path_style` picks the addressing: `https://host/bucket` (true, what most
/// self-hosted stores expect) or virtual-hosted `https://bucket.host` (false,
/// required by e.g. Alibaba OSS). With virtual-hosted the SDK falls back to
/// path-style for bucket names that can't be a DNS label.
pub fn build_client(
    endpoint: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    path_style: bool,
) -> Result<Client> {
    let host = endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');

    let creds = Credentials::new(access_key, secret_key, None, None, "pier");
    let region = aws_sdk_s3::config::Region::new(region.to_string());

    // `WhenRequired`: by default the SDK adds a CRC32 to every upload, and for
    // a streamed body (export archives, `upload_stream`) it sends it as an
    // aws-chunked trailer (`STREAMING-UNSIGNED-PAYLOAD-TRAILER`). Many
    // S3-compatible stores (Ceph RGW, older MinIO, …) don't support that and
    // answer `XAmzContentSHA256Mismatch`. Without it a streamed body goes as
    // `UNSIGNED-PAYLOAD` over TLS, and an in-memory one keeps its signed
    // SHA-256 payload hash, which the store verifies.
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(region)
        .endpoint_url(format!("https://{host}"))
        .credentials_provider(creds)
        .force_path_style(path_style)
        .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
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
    path_style: bool,
) -> Result<()> {
    let client = build_client(endpoint, region, access_key, secret_key, path_style)?;
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

/// Archives up to this size go up in one `PutObject`; larger ones as a
/// multipart upload (a single PUT is capped at 5 GB on S3).
pub const MULTIPART_THRESHOLD: u64 = 64 * MIB;
const MIB: u64 = 1024 * 1024;
/// S3 allows at most 10 000 parts; stay well under it.
const MAX_PARTS: u64 = 9_000;

/// Part size for a multipart upload of `size` bytes: 64 MiB, or more for
/// huge files so the part count stays under [`MAX_PARTS`]. Whole MiB, and
/// far above S3's 5 MiB minimum part size.
pub fn part_size(size: u64) -> u64 {
    let min_for_count = size.div_ceil(MAX_PARTS).div_ceil(MIB) * MIB;
    min_for_count.max(MULTIPART_THRESHOLD)
}

/// Upload a file to S3 straight from disk, without reading it into memory
/// as a whole. Used for export archives, which may carry large DB dumps.
/// `size` is the file length; above [`MULTIPART_THRESHOLD`] the file goes up
/// in parts, so archives beyond the 5 GB single-PUT limit work too.
pub async fn upload_stream(
    client: &Client,
    bucket: &str,
    key: &str,
    file: tokio::fs::File,
    size: u64,
) -> Result<()> {
    if size <= MULTIPART_THRESHOLD {
        let body = aws_sdk_s3::primitives::ByteStream::read_from()
            .file(file)
            .build()
            .await?;
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await?;
        return Ok(());
    }

    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await?
        .upload_id
        .ok_or_else(|| anyhow!("S3 returned no upload id"))?;

    match upload_parts(client, bucket, key, &upload_id, file, size).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Abandoned parts stay billed in the bucket until aborted.
            if let Err(abort) = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                tracing::warn!("abort multipart upload of {key} failed: {abort}");
            }
            Err(e)
        }
    }
}

/// Send `file` as consecutive parts of `upload_id` and complete the upload.
/// One part is held in memory at a time; an in-memory body lets the SDK
/// retry a part on its own and signs its SHA-256, which the store verifies.
async fn upload_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    mut file: tokio::fs::File,
    size: u64,
) -> Result<()> {
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    use tokio::io::AsyncReadExt;

    let part = part_size(size);
    let mut parts = Vec::new();
    let mut sent = 0u64;
    let mut number = 1i32;
    while sent < size {
        let len = part.min(size - sent) as usize;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;
        let resp = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(buf.into())
            .send()
            .await?;
        parts.push(
            CompletedPart::builder()
                .part_number(number)
                .set_e_tag(resp.e_tag)
                .build(),
        );
        sent += len as u64;
        number += 1;
    }
    client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await?;
    Ok(())
}

/// Stream an object out of storage (S3-compatible or Bunny) without
/// buffering it. Returns the length when the store reports it.
#[allow(clippy::too_many_arguments)]
pub async fn download_stream(
    storage_type: &str,
    endpoint: &str,
    region: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    path_style: bool,
    key: &str,
) -> Result<(Option<u64>, axum::body::Body)> {
    match storage_type {
        "bunny" => bunny::download_stream(bucket, access_key, endpoint, key).await,
        _ => {
            let client = build_client(endpoint, region, access_key, secret_key, path_style)?;
            let resp = client.get_object().bucket(bucket).key(key).send().await?;
            let len = resp.content_length.and_then(|n| u64::try_from(n).ok());
            let reader = resp.body.into_async_read();
            Ok((
                len,
                axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader)),
            ))
        }
    }
}

/// Delete an object from S3.
pub async fn delete_object(client: &Client, bucket: &str, key: &str) -> Result<()> {
    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await?;
    Ok(())
}

/// Delete a blob from whichever storage backend (S3-compatible or Bunny)
/// the passed config points at. Used by the Backup-delete API and by
/// retention cleanup so orphaned blobs don't accumulate.
#[allow(clippy::too_many_arguments)]
pub async fn delete_blob(
    storage_type: &str,
    endpoint: &str,
    region: &str,
    bucket: &str,
    access_key: &str,
    secret_key: &str,
    path_style: bool,
    key: &str,
) -> Result<()> {
    match storage_type {
        "bunny" => bunny::delete_file(bucket, access_key, endpoint, key).await,
        _ => {
            let client = build_client(endpoint, region, access_key, secret_key, path_style)?;
            delete_object(&client, bucket, key).await
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_archives_use_the_minimum_part() {
        assert_eq!(part_size(100 * MIB), 64 * MIB);
        assert_eq!(part_size(6 * 1024 * MIB), 64 * MIB);
    }

    #[test]
    fn huge_archives_stay_under_the_part_limit() {
        for size in [
            575 * 1024 * MIB,
            576 * 1024 * MIB,
            1024 * 1024 * MIB,
            5 * 1024 * 1024 * MIB,
        ] {
            let p = part_size(size);
            assert!(size.div_ceil(p) <= MAX_PARTS, "size {size}: part {p}");
            assert_eq!(p % MIB, 0);
            assert!(p >= 5 * MIB);
        }
    }
}
