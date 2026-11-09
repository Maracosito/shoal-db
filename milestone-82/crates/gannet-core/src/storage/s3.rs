//! S3-compatible [`ObjectStore`] backend (AWS S3, MinIO, and other
//! S3-API-compatible services).
//!
//! `put_if_not_exists` relies on conditional writes (`If-None-Match: *`),
//! which are supported by AWS S3 (since late 2024) and by MinIO. Backends
//! without conditional-write support are not suitable for running Gannet's
//! manifest commit protocol and will fail the conformance suite.

use std::ops::Range;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::Client;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{StorageError, StorageResult};

use super::{validate_key, validate_prefix, ObjectMeta, ObjectStore};

/// Configuration for an S3-compatible backend.
///
/// For MinIO in local development:
///
/// ```yaml
/// backend: s3
/// bucket: gannet
/// endpoint: http://localhost:9000
/// region: us-east-1
/// access_key_id: minioadmin
/// secret_access_key: minioadmin
/// force_path_style: true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// Bucket name. Must already exist (or be created via
    /// [`S3Store::ensure_bucket`] in dev/test environments).
    pub bucket: String,
    /// Optional key prefix under which all of this store's objects live.
    #[serde(default)]
    pub prefix: Option<String>,
    /// AWS region. Defaults to `us-east-1` when a custom endpoint is set.
    #[serde(default)]
    pub region: Option<String>,
    /// Custom endpoint URL for S3-compatible services (e.g. MinIO).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Static credentials. If unset, the default AWS credential chain
    /// (environment, profile, IMDS, …) is used. Never logged.
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    /// Use path-style addressing (`endpoint/bucket/key`). Required by MinIO.
    /// Defaults to `true` when `endpoint` is set, `false` otherwise.
    #[serde(default)]
    pub force_path_style: Option<bool>,
}

/// An [`ObjectStore`] backed by an S3-compatible service.
#[derive(Debug, Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
    /// Normalized prefix: empty, or ends with exactly one `/`.
    prefix: String,
}

fn normalize_prefix(prefix: Option<&str>) -> String {
    let p = prefix.unwrap_or("").trim_matches('/');
    if p.is_empty() {
        String::new()
    } else {
        format!("{p}/")
    }
}

fn to_system_time(dt: &DateTime) -> Option<SystemTime> {
    let millis = dt.to_millis().ok()?;
    if millis < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(millis as u64))
}

fn backend_err<E: std::fmt::Debug>(key: &str, e: E) -> StorageError {
    StorageError::Backend {
        key: key.into(),
        message: format!("{e:?}"),
    }
}

/// Extract the HTTP status code from an SDK service error, if any.
fn service_status<E, R>(err: &SdkError<E, R>) -> Option<u16>
where
    R: std::fmt::Debug,
    E: std::fmt::Debug,
{
    if let SdkError::ServiceError(_) = err {
        // `raw()` is only available on the ServiceError variant; route through
        // the typed accessor to stay compatible with smithy-runtime updates.
        err.raw_response().map(|r| r.status().as_u16())
    } else {
        None
    }
}

impl S3Store {
    /// Build a store from configuration, constructing the AWS SDK client.
    pub async fn from_config(cfg: &S3Config) -> StorageResult<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());

        let region = cfg
            .region
            .clone()
            .or_else(|| cfg.endpoint.as_ref().map(|_| "us-east-1".to_string()));
        if let Some(region) = region {
            loader = loader.region(aws_config::Region::new(region));
        }
        if let Some(endpoint) = &cfg.endpoint {
            loader = loader.endpoint_url(endpoint.clone());
        }
        if let (Some(ak), Some(sk)) = (&cfg.access_key_id, &cfg.secret_access_key) {
            let creds = aws_credential_types::Credentials::from_keys(ak.clone(), sk.clone(), None);
            loader = loader.credentials_provider(creds);
        }

        let shared = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        let force_path_style = cfg.force_path_style.unwrap_or(cfg.endpoint.is_some());
        builder = builder.force_path_style(force_path_style);
        let client = Client::from_conf(builder.build());

        Ok(Self {
            client,
            bucket: cfg.bucket.clone(),
            prefix: normalize_prefix(cfg.prefix.as_deref()),
        })
    }

    /// Wrap an existing client (useful for tests and custom setups).
    pub fn with_client(client: Client, bucket: impl Into<String>, prefix: Option<&str>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            prefix: normalize_prefix(prefix),
        }
    }

    /// Create the bucket if it does not already exist. Intended for dev and
    /// test environments (MinIO); production buckets should be provisioned
    /// out of band with appropriate policies.
    pub async fn ensure_bucket(&self) -> StorageResult<()> {
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                if let SdkError::ServiceError(se) = &e {
                    let err = se.err();
                    if err.is_bucket_already_owned_by_you() || err.is_bucket_already_exists() {
                        return Ok(());
                    }
                }
                Err(backend_err(&self.bucket, e))
            }
        }
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.prefix, key)
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        validate_key(key)?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| backend_err(key, e))?;
        Ok(())
    }

    async fn put_if_not_exists(&self, key: &str, data: Bytes) -> StorageResult<bool> {
        validate_key(key)?;
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .if_none_match("*")
            .body(ByteStream::from(data))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                // 412 PreconditionFailed: object already exists.
                // 409 ConditionalRequestConflict: a concurrent conditional
                // write raced us; treat as "exists" — the caller must re-read.
                if matches!(service_status(&e), Some(412) | Some(409)) {
                    return Ok(false);
                }
                Err(backend_err(key, e))
            }
        }
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        validate_key(key)?;
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
        {
            Ok(resp) => resp
                .body
                .collect()
                .await
                .map(|agg| agg.into_bytes())
                .map_err(|e| backend_err(key, e)),
            Err(e) => {
                if let SdkError::ServiceError(se) = &e {
                    if se.err().is_no_such_key() {
                        return Err(StorageError::NotFound { key: key.into() });
                    }
                }
                if service_status(&e) == Some(404) {
                    return Err(StorageError::NotFound { key: key.into() });
                }
                Err(backend_err(key, e))
            }
        }
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> StorageResult<Bytes> {
        validate_key(key)?;
        if range.start >= range.end {
            return Ok(Bytes::new());
        }
        // HTTP ranges are inclusive on both ends.
        let header = format!("bytes={}-{}", range.start, range.end - 1);
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .range(header)
            .send()
            .await
        {
            Ok(resp) => resp
                .body
                .collect()
                .await
                .map(|agg| agg.into_bytes())
                .map_err(|e| backend_err(key, e)),
            Err(e) => {
                if let SdkError::ServiceError(se) = &e {
                    if se.err().is_no_such_key() {
                        return Err(StorageError::NotFound { key: key.into() });
                    }
                }
                match service_status(&e) {
                    Some(404) => Err(StorageError::NotFound { key: key.into() }),
                    Some(416) => Err(StorageError::backend(
                        key,
                        format!("range start {} is at or past object size", range.start),
                    )),
                    _ => Err(backend_err(key, e)),
                }
            }
        }
    }

    async fn head(&self, key: &str) -> StorageResult<ObjectMeta> {
        validate_key(key)?;
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
        {
            Ok(resp) => Ok(ObjectMeta {
                key: key.to_string(),
                size: resp.content_length().unwrap_or(0).max(0) as u64,
                last_modified: resp.last_modified().and_then(to_system_time),
                etag: resp.e_tag().map(str::to_string),
            }),
            Err(e) => {
                if let SdkError::ServiceError(se) = &e {
                    if se.err().is_not_found() {
                        return Err(StorageError::NotFound { key: key.into() });
                    }
                }
                if service_status(&e) == Some(404) {
                    return Err(StorageError::NotFound { key: key.into() });
                }
                Err(backend_err(key, e))
            }
        }
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        validate_key(key)?;
        // S3 DeleteObject succeeds for missing keys, matching our idempotent
        // delete contract.
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(|e| backend_err(key, e))?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> StorageResult<Vec<ObjectMeta>> {
        validate_prefix(prefix)?;
        let full_prefix = format!("{}{}", self.prefix, prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&full_prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.map_err(|e| backend_err(prefix, e))?;
            for obj in resp.contents() {
                let Some(full_key) = obj.key() else { continue };
                let Some(key) = full_key.strip_prefix(self.prefix.as_str()) else {
                    continue;
                };
                out.push(ObjectMeta {
                    key: key.to_string(),
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    last_modified: obj.last_modified().and_then(to_system_time),
                    etag: obj.e_tag().map(str::to_string),
                });
            }
            if resp.is_truncated() == Some(true) {
                token = resp.next_continuation_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        // ListObjectsV2 returns keys in UTF-8 binary order already, but sort
        // defensively so the contract holds across S3-compatible services.
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn copy(&self, src: &str, dst: &str) -> StorageResult<()> {
        validate_key(src)?;
        validate_key(dst)?;
        // Keys are restricted to [A-Za-z0-9._/-], so no URL-encoding of the
        // copy source is required.
        let copy_source = format!("{}/{}", self.bucket, self.full_key(src));
        match self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(copy_source)
            .key(self.full_key(dst))
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                if service_status(&e) == Some(404) {
                    return Err(StorageError::NotFound { key: src.into() });
                }
                Err(backend_err(src, e))
            }
        }
    }
}
