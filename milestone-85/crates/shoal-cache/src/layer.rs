//! Composed two-tier cache in front of an origin fetch.
//!
//! Lookup order: memory -> disk -> origin closure. Successful origin fetches
//! populate both tiers (memory only when the payload fits under the configured
//! per-item cap, so a single huge segment cannot flush the hot set).

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;

use crate::disk::DiskCache;
use crate::memory::MemoryCache;
use crate::stats::{LayerSnapshot, TierStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheTier {
    Memory,
    Disk,
    Origin,
}

impl CacheTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheTier::Memory => "memory",
            CacheTier::Disk => "disk",
            CacheTier::Origin => "origin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheLayerConfig {
    /// Total memory-tier budget in bytes.
    pub memory_max_bytes: u64,
    /// Per-item cap for the memory tier; larger payloads only live on disk.
    pub memory_max_item_bytes: u64,
}

impl Default for CacheLayerConfig {
    fn default() -> Self {
        Self {
            memory_max_bytes: 256 * 1024 * 1024,
            memory_max_item_bytes: 8 * 1024 * 1024,
        }
    }
}

pub struct CacheLayer {
    mem: MemoryCache<Bytes>,
    disk: Option<Arc<DiskCache>>,
    max_item: u64,
    stats: TierStats,
}

impl CacheLayer {
    pub fn new(cfg: CacheLayerConfig, disk: Option<Arc<DiskCache>>) -> Self {
        Self {
            mem: MemoryCache::new(cfg.memory_max_bytes, |b: &Bytes| b.len() as u64),
            disk,
            max_item: cfg.memory_max_item_bytes,
            stats: TierStats::default(),
        }
    }

    fn maybe_promote_to_memory(&self, key: &str, bytes: &Bytes) {
        if (bytes.len() as u64) <= self.max_item {
            self.mem.put(key, bytes.clone());
        }
    }

    /// Look up `key`, falling back to `fetch` on a full miss. Returns the
    /// bytes and the tier that served them (for metrics). Cache-population
    /// failures are logged but never fail the request; only `fetch` errors
    /// propagate.
    pub async fn get_or_fetch<F, Fut, E>(&self, key: &str, fetch: F) -> Result<(Bytes, CacheTier), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, E>>,
    {
        if let Some(v) = self.mem.get(key) {
            self.stats.memory_hit();
            return Ok(((*v).clone(), CacheTier::Memory));
        }
        if let Some(disk) = &self.disk {
            if let Some(b) = disk.get(key).await {
                self.stats.disk_hit();
                self.maybe_promote_to_memory(key, &b);
                return Ok((b, CacheTier::Disk));
            }
        }
        let bytes = fetch().await?;
        self.stats.origin_fetch(bytes.len() as u64);
        if let Some(disk) = &self.disk {
            if let Err(e) = disk.put(key, bytes.clone()).await {
                tracing::warn!(key, error = %e, "failed to populate disk cache");
            }
        }
        self.maybe_promote_to_memory(key, &bytes);
        Ok((bytes, CacheTier::Origin))
    }

    /// Check tiers without falling back to the origin.
    pub async fn get_cached(&self, key: &str) -> Option<(Bytes, CacheTier)> {
        if let Some(v) = self.mem.get(key) {
            self.stats.memory_hit();
            return Some(((*v).clone(), CacheTier::Memory));
        }
        if let Some(disk) = &self.disk {
            if let Some(b) = disk.get(key).await {
                self.stats.disk_hit();
                self.maybe_promote_to_memory(key, &b);
                return Some((b, CacheTier::Disk));
            }
        }
        None
    }

    /// Pre-populate both tiers (used by the warm-cache endpoint).
    pub async fn warm(&self, key: &str, bytes: Bytes) -> std::io::Result<()> {
        if let Some(disk) = &self.disk {
            disk.put(key, bytes.clone()).await?;
        }
        self.maybe_promote_to_memory(key, &bytes);
        Ok(())
    }

    /// Drop all entries under a key prefix from both tiers.
    pub async fn invalidate_prefix(&self, prefix: &str) -> usize {
        let mem = self.mem.remove_prefix(prefix);
        let disk = match &self.disk {
            Some(d) => d.remove_prefix(prefix).await,
            None => 0,
        };
        mem + disk
    }

    /// Pin a key prefix in both tiers (disk pins persist across restarts).
    pub fn pin_prefix(&self, prefix: &str) -> std::io::Result<()> {
        self.mem.pin_prefix(prefix);
        if let Some(disk) = &self.disk {
            disk.pin_prefix(prefix)?;
        }
        Ok(())
    }

    pub fn unpin_prefix(&self, prefix: &str) -> std::io::Result<()> {
        self.mem.unpin_prefix(prefix);
        if let Some(disk) = &self.disk {
            disk.unpin_prefix(prefix)?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> LayerSnapshot {
        LayerSnapshot {
            tiers: self.stats.snapshot(),
            memory: self.mem.snapshot_counters(),
            memory_entries: self.mem.entries(),
            memory_bytes: self.mem.total_weight(),
            disk: self.disk.as_ref().map(|d| d.snapshot()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskCacheConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn origin_then_memory() {
        let layer = CacheLayer::new(CacheLayerConfig::default(), None);
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"payload"))
        };
        let (b, t) = layer.get_or_fetch("k", fetch).await.unwrap();
        assert_eq!(b, Bytes::from_static(b"payload"));
        assert_eq!(t, CacheTier::Origin);

        let fetch2 = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"payload"))
        };
        let (_, t2) = layer.get_or_fetch("k", fetch2).await.unwrap();
        assert_eq!(t2, CacheTier::Memory);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let snap = layer.snapshot();
        assert_eq!(snap.tiers.origin_fetches, 1);
        assert_eq!(snap.tiers.memory_hits, 1);
    }

    #[tokio::test]
    async fn disk_tier_serves_when_memory_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(
            DiskCache::open(DiskCacheConfig {
                root: dir.path().to_path_buf(),
                max_bytes: 1 << 20,
            })
            .unwrap(),
        );
        // memory_max_item_bytes = 0 -> nothing is memory-cached.
        let layer = CacheLayer::new(
            CacheLayerConfig {
                memory_max_bytes: 1 << 20,
                memory_max_item_bytes: 0,
            },
            Some(disk),
        );
        let (_, t1) = layer
            .get_or_fetch("k", || async { Ok::<_, std::io::Error>(Bytes::from_static(b"x")) })
            .await
            .unwrap();
        assert_eq!(t1, CacheTier::Origin);
        let (_, t2) = layer
            .get_or_fetch("k", || async { panic!("origin must not be called") })
            .await
            .map_err(|_: std::io::Error| ())
            .unwrap();
        assert_eq!(t2, CacheTier::Disk);
    }

    #[tokio::test]
    async fn fetch_errors_propagate_and_are_not_cached() {
        let layer = CacheLayer::new(CacheLayerConfig::default(), None);
        let err = layer
            .get_or_fetch("k", || async {
                Err::<Bytes, _>(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"))
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(layer.get_cached("k").await.is_none());
    }

    #[tokio::test]
    async fn warm_and_invalidate() {
        let layer = CacheLayer::new(CacheLayerConfig::default(), None);
        layer.warm("ns/a/seg", Bytes::from_static(b"warm")).await.unwrap();
        assert!(layer.get_cached("ns/a/seg").await.is_some());
        assert_eq!(layer.invalidate_prefix("ns/a/").await, 1);
        assert!(layer.get_cached("ns/a/seg").await.is_none());
    }
}
