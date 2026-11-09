//! In-process object store used by unit tests and as a reference
//! implementation of the [`ObjectStore`] semantics.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::RwLock;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{StorageError, StorageResult};

use super::{validate_key, validate_prefix, ObjectMeta, ObjectStore};

#[derive(Debug, Clone)]
struct Entry {
    data: Bytes,
    modified: SystemTime,
}

/// An [`ObjectStore`] holding everything in process memory.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: RwLock<BTreeMap<String, Entry>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(data: Bytes) -> Entry {
        Entry {
            data,
            modified: SystemTime::now(),
        }
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        validate_key(key)?;
        let mut map = self.inner.write().expect("memory store lock poisoned");
        map.insert(key.to_string(), Self::entry(data));
        Ok(())
    }

    async fn put_if_not_exists(&self, key: &str, data: Bytes) -> StorageResult<bool> {
        validate_key(key)?;
        let mut map = self.inner.write().expect("memory store lock poisoned");
        if map.contains_key(key) {
            return Ok(false);
        }
        map.insert(key.to_string(), Self::entry(data));
        Ok(true)
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        validate_key(key)?;
        let map = self.inner.read().expect("memory store lock poisoned");
        map.get(key)
            .map(|e| e.data.clone())
            .ok_or_else(|| StorageError::NotFound { key: key.into() })
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> StorageResult<Bytes> {
        validate_key(key)?;
        if range.start >= range.end {
            // Degenerate range: defined to return empty without a backend call.
            return Ok(Bytes::new());
        }
        let map = self.inner.read().expect("memory store lock poisoned");
        let entry = map
            .get(key)
            .ok_or_else(|| StorageError::NotFound { key: key.into() })?;
        let len = entry.data.len() as u64;
        if range.start >= len {
            return Err(StorageError::backend(
                key,
                format!("range start {} is at or past object size {len}", range.start),
            ));
        }
        let end = range.end.min(len);
        Ok(entry.data.slice(range.start as usize..end as usize))
    }

    async fn head(&self, key: &str) -> StorageResult<ObjectMeta> {
        validate_key(key)?;
        let map = self.inner.read().expect("memory store lock poisoned");
        let entry = map
            .get(key)
            .ok_or_else(|| StorageError::NotFound { key: key.into() })?;
        Ok(ObjectMeta {
            key: key.to_string(),
            size: entry.data.len() as u64,
            last_modified: Some(entry.modified),
            etag: None,
        })
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        validate_key(key)?;
        let mut map = self.inner.write().expect("memory store lock poisoned");
        map.remove(key); // idempotent: absent key is fine
        Ok(())
    }

    async fn list(&self, prefix: &str) -> StorageResult<Vec<ObjectMeta>> {
        validate_prefix(prefix)?;
        let map = self.inner.read().expect("memory store lock poisoned");
        let mut out = Vec::new();
        // BTreeMap is ordered, so range-scan from the prefix and stop at the
        // first non-matching key.
        for (key, entry) in map.range(prefix.to_string()..) {
            if !key.starts_with(prefix) {
                break;
            }
            out.push(ObjectMeta {
                key: key.clone(),
                size: entry.data.len() as u64,
                last_modified: Some(entry.modified),
                etag: None,
            });
        }
        Ok(out)
    }

    async fn copy(&self, src: &str, dst: &str) -> StorageResult<()> {
        validate_key(src)?;
        validate_key(dst)?;
        let mut map = self.inner.write().expect("memory store lock poisoned");
        let data = map
            .get(src)
            .map(|e| e.data.clone())
            .ok_or_else(|| StorageError::NotFound { key: src.into() })?;
        map.insert(dst.to_string(), Self::entry(data));
        Ok(())
    }
}
