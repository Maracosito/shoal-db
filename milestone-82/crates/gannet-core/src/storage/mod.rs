//! The object-storage abstraction every higher layer is built on.
//!
//! Durable state in Gannet lives exclusively behind [`ObjectStore`]. The
//! trait is intentionally small — the minimal set of primitives required to
//! implement WAL commits, manifest compare-and-set fencing, immutable segment
//! reads (including byte-range reads for partial index fetches), and garbage
//! collection. Anything not expressible against this trait does not belong in
//! the engine.

mod config;
mod fs;
mod memory;
mod s3;

pub use config::{open_store, StorageConfig};
pub use fs::FilesystemStore;
pub use memory::MemoryStore;
pub use s3::{S3Config, S3Store};

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{StorageError, StorageResult};

/// Metadata about a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    /// Full key of the object (relative to the store root / prefix).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Last-modified time, if the backend reports one.
    pub last_modified: Option<SystemTime>,
    /// Backend entity tag, if available (S3 ETag). Not comparable across backends.
    pub etag: Option<String>,
}

/// A shared handle to an object store.
pub type DynObjectStore = Arc<dyn ObjectStore>;

/// The storage primitive Gannet is built on.
///
/// ### Required semantics (verified by the conformance test suite)
///
/// - **Atomic put** — concurrent readers see either the previous object or
///   the complete new object, never a torn write.
/// - **`put_if_not_exists`** — atomically creates the object only if absent;
///   returns `Ok(false)` (without writing) if it already exists. This is the
///   fencing primitive for manifest commits.
/// - **Idempotent delete** — deleting a missing object returns `Ok(())`.
/// - **Ordered list** — `list` returns objects sorted lexicographically by key.
/// - **Range reads** — `get_range` uses a half-open `[start, end)` byte
///   range. An `end` past the object length is clamped; a `start` at or past
///   the object length (for a non-empty range) is an error. A degenerate
///   range (`start == end`) returns empty bytes without touching the backend.
#[async_trait]
pub trait ObjectStore: Send + Sync + fmt::Debug + 'static {
    /// Write (or overwrite) an object atomically.
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()>;

    /// Atomically create the object only if it does not already exist.
    /// Returns `Ok(true)` if this call created the object, `Ok(false)` if an
    /// object with this key already existed (nothing is written in that case).
    async fn put_if_not_exists(&self, key: &str, data: Bytes) -> StorageResult<bool>;

    /// Fetch an entire object.
    async fn get(&self, key: &str) -> StorageResult<Bytes>;

    /// Fetch a half-open byte range `[range.start, range.end)` of an object.
    async fn get_range(&self, key: &str, range: Range<u64>) -> StorageResult<Bytes>;

    /// Fetch object metadata without the body.
    async fn head(&self, key: &str) -> StorageResult<ObjectMeta>;

    /// Delete an object. Deleting a missing object is not an error.
    async fn delete(&self, key: &str) -> StorageResult<()>;

    /// List all objects whose key starts with `prefix`, sorted by key.
    async fn list(&self, prefix: &str) -> StorageResult<Vec<ObjectMeta>>;

    /// Server-side (or emulated) copy of `src` to `dst`, overwriting `dst`.
    async fn copy(&self, src: &str, dst: &str) -> StorageResult<()>;
}

fn allowed_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

/// Validate an object key against Gannet's portable key rules.
///
/// Keys must be 1–1024 characters from `[A-Za-z0-9._/-]`, must not start or
/// end with `/`, must not contain empty segments (`//`), and no segment may
/// begin with `.` (this both forbids `..` traversal and reserves dot-prefixed
/// names for backend-internal use, e.g. the filesystem backend's temp dir).
pub fn validate_key(key: &str) -> StorageResult<()> {
    if key.is_empty() {
        return Err(StorageError::InvalidKey("key must not be empty".into()));
    }
    if key.len() > 1024 {
        return Err(StorageError::InvalidKey(format!(
            "key exceeds 1024 bytes: {key}"
        )));
    }
    if key.starts_with('/') || key.ends_with('/') {
        return Err(StorageError::InvalidKey(format!(
            "key must not start or end with '/': {key}"
        )));
    }
    if let Some(bad) = key.chars().find(|c| !allowed_key_char(*c)) {
        return Err(StorageError::InvalidKey(format!(
            "key contains disallowed character {bad:?}: {key}"
        )));
    }
    for segment in key.split('/') {
        if segment.is_empty() {
            return Err(StorageError::InvalidKey(format!(
                "key contains empty segment: {key}"
            )));
        }
        if segment.starts_with('.') {
            return Err(StorageError::InvalidKey(format!(
                "key segment must not start with '.': {key}"
            )));
        }
    }
    Ok(())
}

/// Validate a list prefix. A prefix may be empty, may end with `/`, and its
/// final segment may be a partial key segment; otherwise the same rules as
/// [`validate_key`] apply.
pub fn validate_prefix(prefix: &str) -> StorageResult<()> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.starts_with('/') {
        return Err(StorageError::InvalidKey(format!(
            "prefix must not start with '/': {prefix}"
        )));
    }
    if let Some(bad) = prefix.chars().find(|c| !allowed_key_char(*c)) {
        return Err(StorageError::InvalidKey(format!(
            "prefix contains disallowed character {bad:?}: {prefix}"
        )));
    }
    let core = prefix.strip_suffix('/').unwrap_or(prefix);
    for segment in core.split('/') {
        if segment.is_empty() {
            return Err(StorageError::InvalidKey(format!(
                "prefix contains empty segment: {prefix}"
            )));
        }
        if segment.starts_with('.') {
            return Err(StorageError::InvalidKey(format!(
                "prefix segment must not start with '.': {prefix}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert!(validate_key("ns/docs/wal/00000001.wal").is_ok());
        assert!(validate_key("a").is_ok());
        assert!(validate_key("a-b_c.d/e").is_ok());

        assert!(validate_key("").is_err());
        assert!(validate_key("/abs").is_err());
        assert!(validate_key("trailing/").is_err());
        assert!(validate_key("a//b").is_err());
        assert!(validate_key("a/../b").is_err());
        assert!(validate_key(".hidden").is_err());
        assert!(validate_key("a/.tmp/b").is_err());
        assert!(validate_key("space key").is_err());
        assert!(validate_key("uni\u{e9}").is_err());
    }

    #[test]
    fn prefix_validation() {
        assert!(validate_prefix("").is_ok());
        assert!(validate_prefix("ns/").is_ok());
        assert!(validate_prefix("ns/docs/wal/000").is_ok());

        assert!(validate_prefix("/ns").is_err());
        assert!(validate_prefix("a//b").is_err());
        assert!(validate_prefix("a/../").is_err());
        assert!(validate_prefix(".tmp/").is_err());
    }
}
