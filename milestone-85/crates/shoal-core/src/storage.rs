//! Object storage abstraction.
//!
//! Wraps the `object_store` crate so the rest of the engine is agnostic to
//! backend: in-memory (tests), local filesystem (dev / single node), or any
//! S3-compatible store (AWS S3, MinIO, R2, GCS-interop, ...).
//!
//! All operations update atomic I/O counters that the server exports as
//! Prometheus metrics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, PutMode, PutOptions, PutPayload};

use crate::error::{Result, ShoalError};

#[derive(Debug, Default)]
pub struct StorageStats {
    pub get_ops: AtomicU64,
    pub put_ops: AtomicU64,
    pub delete_ops: AtomicU64,
    pub list_ops: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct StorageStatsSnapshot {
    pub get_ops: u64,
    pub put_ops: u64,
    pub delete_ops: u64,
    pub list_ops: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, Default)]
pub struct S3Config {
    pub bucket: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Allow plain-HTTP endpoints (MinIO in dev).
    pub allow_http: bool,
}

pub struct ObjectStorage {
    store: Arc<dyn ObjectStore>,
    stats: StorageStats,
    backend: &'static str,
}

impl std::fmt::Debug for ObjectStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStorage")
            .field("backend", &self.backend)
            .finish()
    }
}

impl ObjectStorage {
    pub fn new_memory() -> Self {
        ObjectStorage {
            store: Arc::new(InMemory::new()),
            stats: StorageStats::default(),
            backend: "memory",
        }
    }

    pub fn new_local(path: impl AsRef<std::path::Path>) -> Result<Self> {
        std::fs::create_dir_all(path.as_ref())?;
        let store = LocalFileSystem::new_with_prefix(path.as_ref())
            .map_err(|e| ShoalError::Storage(e.to_string()))?;
        Ok(ObjectStorage {
            store: Arc::new(store),
            stats: StorageStats::default(),
            backend: "local",
        })
    }

    pub fn new_s3(cfg: S3Config) -> Result<Self> {
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&cfg.bucket)
            .with_conditional_put(S3ConditionalPut::ETagMatch);
        if let Some(region) = &cfg.region {
            builder = builder.with_region(region);
        }
        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        if let Some(ak) = &cfg.access_key_id {
            builder = builder.with_access_key_id(ak);
        }
        if let Some(sk) = &cfg.secret_access_key {
            builder = builder.with_secret_access_key(sk);
        }
        if cfg.allow_http {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|e| ShoalError::Storage(e.to_string()))?;
        Ok(ObjectStorage {
            store: Arc::new(store),
            stats: StorageStats::default(),
            backend: "s3",
        })
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    pub async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        let p = ObjPath::from(key);
        self.stats.put_ops.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_written
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.store.put(&p, PutPayload::from(data)).await?;
        Ok(())
    }

    /// Create-if-absent. Returns `ShoalError::AlreadyExists` if the key exists.
    ///
    /// Uses the backend's native conditional put where available; falls back
    /// to an existence check + put (with a documented race window) when the
    /// backend reports the operation as unimplemented.
    pub async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<()> {
        let p = ObjPath::from(key);
        self.stats.put_ops.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_written
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .store
            .put_opts(&p, PutPayload::from(data.clone()), opts)
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { path, .. }) => {
                Err(ShoalError::AlreadyExists(path))
            }
            Err(object_store::Error::NotImplemented) => {
                // Fallback for backends without conditional puts. Racy across
                // processes; Shoal's single-writer-per-namespace model makes
                // this safe in practice (see ARCHITECTURE.md / consistency).
                if self.exists(key).await? {
                    return Err(ShoalError::AlreadyExists(key.to_string()));
                }
                self.store.put(&p, PutPayload::from(data)).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Bytes> {
        let p = ObjPath::from(key);
        self.stats.get_ops.fetch_add(1, Ordering::Relaxed);
        let res = self.store.get(&p).await?;
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ShoalError::Storage(e.to_string()))?;
        self.stats
            .bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    pub async fn get_opt(&self, key: &str) -> Result<Option<Bytes>> {
        match self.get(key).await {
            Ok(b) => Ok(Some(b)),
            Err(ShoalError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let p = ObjPath::from(key);
        self.stats.delete_ops.fetch_add(1, Ordering::Relaxed);
        match self.store.delete(&p).await {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        let p = ObjPath::from(key);
        match self.store.head(&p).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// List all keys under a prefix (sorted ascending).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.stats.list_ops.fetch_add(1, Ordering::Relaxed);
        let p = ObjPath::from(prefix);
        let metas: Vec<object_store::ObjectMeta> = self
            .store
            .list(Some(&p))
            .try_collect()
            .await
            .map_err(|e| ShoalError::Storage(e.to_string()))?;
        let mut keys: Vec<String> = metas.into_iter().map(|m| m.location.to_string()).collect();
        keys.sort();
        Ok(keys)
    }

    pub fn stats(&self) -> StorageStatsSnapshot {
        StorageStatsSnapshot {
            get_ops: self.stats.get_ops.load(Ordering::Relaxed),
            put_ops: self.stats.put_ops.load(Ordering::Relaxed),
            delete_ops: self.stats.delete_ops.load(Ordering::Relaxed),
            list_ops: self.stats.list_ops.load(Ordering::Relaxed),
            bytes_read: self.stats.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.stats.bytes_written.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_roundtrip_and_conditional() {
        let s = ObjectStorage::new_memory();
        s.put("a/b.txt", Bytes::from_static(b"hi")).await.unwrap();
        assert_eq!(s.get("a/b.txt").await.unwrap(), Bytes::from_static(b"hi"));
        assert!(matches!(
            s.put_if_absent("a/b.txt", Bytes::from_static(b"x")).await,
            Err(ShoalError::AlreadyExists(_))
        ));
        s.put_if_absent("a/c.txt", Bytes::from_static(b"y"))
            .await
            .unwrap();
        let keys = s.list("a/").await.unwrap();
        assert_eq!(keys, vec!["a/b.txt".to_string(), "a/c.txt".to_string()]);
        s.delete("a/b.txt").await.unwrap();
        assert!(s.get_opt("a/b.txt").await.unwrap().is_none());
        assert!(s.stats().put_ops >= 2);
    }

    #[tokio::test]
    async fn local_fs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = ObjectStorage::new_local(dir.path()).unwrap();
        s.put("x/y.bin", Bytes::from_static(b"123")).await.unwrap();
        assert_eq!(s.get("x/y.bin").await.unwrap().len(), 3);
        assert!(s.exists("x/y.bin").await.unwrap());
    }
}
