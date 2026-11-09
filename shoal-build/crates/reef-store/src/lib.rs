//! # reef-store
//!
//! Pluggable object-storage backends for the Reef search database.
//!
//! Everything durable in Reef lives behind the [`ObjectStore`] trait: write-ahead
//! log objects, immutable segments, manifests, and branch references. The engine
//! only ever relies on the following backend semantics:
//!
//! * **Whole-object atomicity.** An object is either fully visible with the bytes
//!   that were written, or not visible at all. Every real object store (S3, GCS,
//!   MinIO) provides this; the local-filesystem backend emulates it with
//!   write-to-temp + atomic rename.
//! * **Conditional create** ([`ObjectStore::put_if_absent`]): a put that fails with
//!   [`StoreError::AlreadyExists`] if the key already exists. This is the only
//!   primitive Reef needs for manifest compare-and-swap and writer fencing. On S3
//!   it is implemented with `If-None-Match: *` (supported by AWS S3 and MinIO).
//! * **Prefix listing** with lexicographic key order.
//!
//! There is deliberately **no rename/overwrite-with-condition requirement** —
//! Reef's storage format is append-only with versioned manifests precisely so it
//! can run on plain S3.
//!
//! ## Backends
//!
//! * [`MemoryStore`] — in-process map, for unit tests.
//! * [`LocalFsStore`] — directory on local disk, for development and tests.
//! * [`S3Store`] *(feature `s3`)* — any S3-compatible service (AWS S3, MinIO,
//!   Cloudflare R2, Ceph RGW) via a dependency-light, hand-rolled SigV4 client.
//! * [`FailpointStore`] — wraps another store and injects deterministic failures
//!   (errors, truncated writes, crashes after N operations) for recovery tests.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

pub mod failpoint;
pub mod localfs;
pub mod memory;
#[cfg(feature = "s3")]
pub mod s3;

pub use failpoint::FailpointStore;
pub use localfs::LocalFsStore;
pub use memory::MemoryStore;
#[cfg(feature = "s3")]
pub use s3::{S3Config, S3Store};

/// Result alias for store operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Errors surfaced by an [`ObjectStore`] backend.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The requested object does not exist.
    #[error("object not found: {0}")]
    NotFound(String),

    /// A conditional create failed because the key already exists.
    #[error("object already exists: {0}")]
    AlreadyExists(String),

    /// A conditional operation's precondition was not met.
    #[error("precondition failed for object: {0}")]
    PreconditionFailed(String),

    /// The backend was misconfigured (bad URL, missing credentials, ...).
    #[error("storage configuration error: {0}")]
    Config(String),

    /// A failure deliberately injected by [`FailpointStore`] during tests.
    #[error("injected failure: {0}")]
    Injected(String),

    /// Underlying I/O error (local filesystem backend).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// Any other backend error (network, server-side, protocol).
    #[error("storage backend error: {0}")]
    Backend(String),
}

impl StoreError {
    /// True if this error means "the object does not exist".
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::NotFound(_))
    }

    /// True if this error means a conditional create lost the race.
    pub fn is_already_exists(&self) -> bool {
        matches!(self, StoreError::AlreadyExists(_))
    }
}

/// Metadata describing a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Store-relative key (any configured bucket prefix is stripped).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Backend entity tag, if available.
    pub etag: Option<String>,
    /// Last-modified time in Unix epoch milliseconds, if available.
    pub last_modified_ms: Option<i64>,
}

/// The storage abstraction every Reef component is written against.
///
/// All methods must be safe to retry: Reef treats every operation as
/// idempotent at the storage layer (puts overwrite with identical content,
/// deletes of missing keys succeed, `put_if_absent` reports a lost race
/// deterministically).
#[async_trait]
pub trait ObjectStore: Send + Sync + fmt::Debug + 'static {
    /// Write (or overwrite) an object atomically.
    async fn put(&self, key: &str, data: Bytes) -> StoreResult<()>;

    /// Create an object only if the key does not already exist.
    ///
    /// Returns [`StoreError::AlreadyExists`] if another writer got there first.
    /// This is the compare-and-swap primitive used for manifest publication.
    async fn put_if_absent(&self, key: &str, data: Bytes) -> StoreResult<()>;

    /// Read an entire object.
    async fn get(&self, key: &str) -> StoreResult<Bytes>;

    /// Read `len` bytes starting at `offset`.
    ///
    /// The default implementation fetches the whole object and slices it;
    /// backends with native range reads (local files, S3 `Range`) override it.
    async fn get_range(&self, key: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        let all = self.get(key).await?;
        let start = usize::try_from(offset)
            .map_err(|_| StoreError::Backend(format!("range offset overflow for {key}")))?;
        if start > all.len() {
            return Err(StoreError::Backend(format!(
                "range start {start} beyond object size {} for {key}",
                all.len()
            )));
        }
        let end = start.saturating_add(len as usize).min(all.len());
        Ok(all.slice(start..end))
    }

    /// Fetch object metadata without reading the body.
    async fn head(&self, key: &str) -> StoreResult<ObjectMeta>;

    /// Delete an object. Deleting a missing key is **not** an error.
    async fn delete(&self, key: &str) -> StoreResult<()>;

    /// List all objects whose keys start with `prefix`, in ascending key order.
    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>>;

    /// Check whether a key exists.
    async fn exists(&self, key: &str) -> StoreResult<bool> {
        match self.head(key).await {
            Ok(_) => Ok(true),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Construct a store from a URL-ish string. Supported forms:
///
/// * `mem://` or `memory://` — in-memory store (non-durable; tests only)
/// * `file:///abs/path` or `file://relative/path` — local filesystem store
/// * a bare path such as `.reef-data` — local filesystem store
/// * `s3://bucket/optional/prefix` — S3-compatible store *(feature `s3`)*;
///   endpoint, region and credentials are taken from the environment
///   (`REEF_S3_ENDPOINT`/`AWS_ENDPOINT_URL`, `REEF_S3_REGION`/`AWS_REGION`,
///   `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`).
pub fn store_from_url(url: &str) -> StoreResult<Arc<dyn ObjectStore>> {
    let url = url.trim();
    if url.is_empty() {
        return Err(StoreError::Config("empty store URL".into()));
    }
    if url == "mem://" || url == "memory://" {
        return Ok(Arc::new(MemoryStore::new()));
    }
    if let Some(rest) = url.strip_prefix("s3://") {
        #[cfg(feature = "s3")]
        {
            let (bucket, prefix) = match rest.split_once('/') {
                Some((b, p)) => (b, p.trim_matches('/')),
                None => (rest, ""),
            };
            if bucket.is_empty() {
                return Err(StoreError::Config(format!("missing bucket in store URL: {url}")));
            }
            let cfg = S3Config::from_env_for(bucket, prefix)?;
            return Ok(Arc::new(S3Store::new(cfg)?));
        }
        #[cfg(not(feature = "s3"))]
        {
            let _ = rest;
            return Err(StoreError::Config(
                "this binary was built without the `s3` feature; rebuild reef-store with `--features s3`"
                    .into(),
            ));
        }
    }
    let path = url.strip_prefix("file://").unwrap_or(url);
    if path.is_empty() {
        return Err(StoreError::Config(format!("empty path in store URL: {url}")));
    }
    Ok(Arc::new(LocalFsStore::new(path)?))
}
