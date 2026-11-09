//! In-memory object store for unit tests.

use crate::{validate_key, validate_prefix, ObjectMeta, ObjectStore, StoreError, StoreResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// An in-process object store backed by a `BTreeMap`. Cloning shares the
/// underlying map (like two clients of the same bucket), which is exactly
/// what restart-simulation tests want: drop the engine, build a new one over
/// the same `MemoryStore` clone, and verify state is recovered.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    inner: Arc<RwLock<BTreeMap<String, Bytes>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored objects (test helper).
    pub fn object_count(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Total stored bytes (test helper).
    pub fn total_bytes(&self) -> u64 {
        self.inner
            .read()
            .unwrap()
            .values()
            .map(|b| b.len() as u64)
            .sum()
    }

    /// Overwrite raw bytes of an object without validation — used by
    /// corruption tests to flip bits in stored WAL/segment files.
    pub fn corrupt_object(&self, path: &str, mutate: impl FnOnce(&mut Vec<u8>)) -> StoreResult<()> {
        let mut map = self.inner.write().unwrap();
        let existing = map
            .get(path)
            .ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        let mut bytes = existing.to_vec();
        mutate(&mut bytes);
        map.insert(path.to_string(), Bytes::from(bytes));
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn put(&self, path: &str, data: Bytes) -> StoreResult<()> {
        validate_key(path)?;
        self.inner.write().unwrap().insert(path.to_string(), data);
        Ok(())
    }

    async fn put_if_absent(&self, path: &str, data: Bytes) -> StoreResult<()> {
        validate_key(path)?;
        let mut map = self.inner.write().unwrap();
        if map.contains_key(path) {
            return Err(StoreError::AlreadyExists(path.to_string()));
        }
        map.insert(path.to_string(), data);
        Ok(())
    }

    async fn get(&self, path: &str) -> StoreResult<Bytes> {
        validate_key(path)?;
        self.inner
            .read()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(path.to_string()))
    }

    async fn get_range(&self, path: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        validate_key(path)?;
        let map = self.inner.read().unwrap();
        let data = map
            .get(path)
            .ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        let size = data.len() as u64;
        if offset > size {
            return Err(StoreError::InvalidRange {
                path: path.to_string(),
                offset,
                len,
                size,
            });
        }
        let end = (offset + len).min(size);
        Ok(data.slice(offset as usize..end as usize))
    }

    async fn head(&self, path: &str) -> StoreResult<ObjectMeta> {
        validate_key(path)?;
        let map = self.inner.read().unwrap();
        let data = map
            .get(path)
            .ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        Ok(ObjectMeta {
            path: path.to_string(),
            size: data.len() as u64,
        })
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>> {
        validate_prefix(prefix)?;
        let map = self.inner.read().unwrap();
        Ok(map
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| ObjectMeta {
                path: k.clone(),
                size: v.len() as u64,
            })
            .collect())
    }

    async fn delete(&self, path: &str) -> StoreResult<()> {
        validate_key(path)?;
        self.inner.write().unwrap().remove(path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_roundtrip() {
        let store = MemoryStore::new();
        store.put("a/b/c.bin", Bytes::from_static(b"hello")).await.unwrap();
        assert_eq!(store.get("a/b/c.bin").await.unwrap(), Bytes::from_static(b"hello"));
        assert!(store.get("a/b/missing").await.unwrap_err().is_not_found());
    }

    #[tokio::test]
    async fn put_if_absent_is_cas() {
        let store = MemoryStore::new();
        store.put_if_absent("k", Bytes::from_static(b"1")).await.unwrap();
        let err = store
            .put_if_absent("k", Bytes::from_static(b"2"))
            .await
            .unwrap_err();
        assert!(err.is_already_exists());
        assert_eq!(store.get("k").await.unwrap(), Bytes::from_static(b"1"));
    }

    #[tokio::test]
    async fn get_range_semantics() {
        let store = MemoryStore::new();
        store.put("k", Bytes::from_static(b"0123456789")).await.unwrap();
        assert_eq!(
            store.get_range("k", 2, 3).await.unwrap(),
            Bytes::from_static(b"234")
        );
        // Clamped past end.
        assert_eq!(
            store.get_range("k", 8, 100).await.unwrap(),
            Bytes::from_static(b"89")
        );
        // Offset at end is empty.
        assert_eq!(store.get_range("k", 10, 1).await.unwrap().len(), 0);
        // Offset past end errors.
        assert!(matches!(
            store.get_range("k", 11, 1).await,
            Err(StoreError::InvalidRange { .. })
        ));
    }

    #[tokio::test]
    async fn list_sorted_by_key() {
        let store = MemoryStore::new();
        for k in ["p/3", "p/1", "p/2", "q/1"] {
            store.put(k, Bytes::from_static(b"x")).await.unwrap();
        }
        let listed = store.list("p/").await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(keys, vec!["p/1", "p/2", "p/3"]);
        assert_eq!(store.list("").await.unwrap().len(), 4);
        assert!(store.list("zz/").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let store = MemoryStore::new();
        store.put("k", Bytes::from_static(b"x")).await.unwrap();
        store.delete("k").await.unwrap();
        store.delete("k").await.unwrap();
        assert!(!store.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn clones_share_state() {
        let a = MemoryStore::new();
        let b = a.clone();
        a.put("k", Bytes::from_static(b"x")).await.unwrap();
        assert!(b.exists("k").await.unwrap());
    }
}
