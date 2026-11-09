//! Local-disk (SSD) cache tier.
//!
//! Layout under the configured root directory:
//!
//! ```text
//! <root>/pins.json                    # persisted pinned key prefixes
//! <root>/objects/<hh>/<sha256>.bin    # raw cached bytes (hh = first 2 hex chars)
//! <root>/objects/<hh>/<sha256>.json   # sidecar: { "key": "...", "len": N }
//! ```
//!
//! Files are content-addressed by the SHA-256 of the *cache key* (not the
//! content), so arbitrary object-storage keys map to safe filenames. The
//! sidecar records the original key so the in-memory index can be rebuilt by
//! scanning the directory on startup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::stats::{CounterSet, DiskTierSnapshot};

#[derive(Debug, Clone)]
pub struct DiskCacheConfig {
    /// Root directory for the cache. Created if missing.
    pub root: PathBuf,
    /// Maximum total bytes of cached data (sum of entry payload sizes).
    /// Pinned entries may push usage above this bound; unpinned entries are
    /// evicted in LRU order to get back under it.
    pub max_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct EntryMeta {
    key: String,
    len: u64,
}

#[derive(Debug)]
struct Entry {
    len: u64,
    tick: u64,
    data_path: PathBuf,
    meta_path: PathBuf,
}

#[derive(Debug, Default)]
struct Index {
    entries: HashMap<String, Entry>,
    /// LRU order: ascending tick = least recently used first.
    order: BTreeMap<u64, String>,
    next_tick: u64,
    total_bytes: u64,
    pinned: HashSet<String>,
}

impl Index {
    fn is_pinned(&self, key: &str) -> bool {
        self.pinned.iter().any(|p| key.starts_with(p.as_str()))
    }

    fn touch(&mut self, key: &str) {
        if let Some(e) = self.entries.get_mut(key) {
            self.order.remove(&e.tick);
            self.next_tick += 1;
            e.tick = self.next_tick;
            self.order.insert(e.tick, key.to_string());
        }
    }

    fn insert(&mut self, key: String, len: u64, data_path: PathBuf, meta_path: PathBuf) {
        if let Some(old) = self.entries.remove(&key) {
            self.order.remove(&old.tick);
            self.total_bytes = self.total_bytes.saturating_sub(old.len);
        }
        self.next_tick += 1;
        self.order.insert(self.next_tick, key.clone());
        self.entries.insert(
            key,
            Entry {
                len,
                tick: self.next_tick,
                data_path,
                meta_path,
            },
        );
        self.total_bytes += len;
    }

    fn forget(&mut self, key: &str) -> Option<(PathBuf, PathBuf)> {
        if let Some(e) = self.entries.remove(key) {
            self.order.remove(&e.tick);
            self.total_bytes = self.total_bytes.saturating_sub(e.len);
            Some((e.data_path, e.meta_path))
        } else {
            None
        }
    }

    /// Evict least-recently-used, non-pinned entries until usage fits.
    /// Returns file paths to delete. Stops early if everything left is pinned.
    fn evict_to_fit(&mut self, max_bytes: u64) -> Vec<(PathBuf, PathBuf)> {
        let mut victims = Vec::new();
        while self.total_bytes > max_bytes {
            let candidate = self
                .order
                .iter()
                .find(|(_, k)| !self.is_pinned(k))
                .map(|(_, k)| k.clone());
            match candidate {
                Some(key) => {
                    if let Some(paths) = self.forget(&key) {
                        victims.push(paths);
                    }
                }
                None => {
                    tracing::warn!(
                        total_bytes = self.total_bytes,
                        max_bytes,
                        "disk cache over budget but all remaining entries are pinned"
                    );
                    break;
                }
            }
        }
        victims
    }
}

pub struct DiskCache {
    cfg: DiskCacheConfig,
    index: Mutex<Index>,
    stats: CounterSet,
    tmp_seq: AtomicU64,
}

impl DiskCache {
    /// Open (or create) a disk cache, rebuilding the index by scanning the
    /// directory. Incomplete or corrupt entries are removed.
    pub fn open(cfg: DiskCacheConfig) -> std::io::Result<Self> {
        let objects = cfg.root.join("objects");
        std::fs::create_dir_all(&objects)?;

        let mut found: Vec<(SystemTime, String, u64, PathBuf, PathBuf)> = Vec::new();
        for shard in std::fs::read_dir(&objects)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(shard.path())? {
                let file = file?;
                let path = file.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                match ext {
                    "tmp" => {
                        // Incomplete write from a previous process.
                        let _ = std::fs::remove_file(&path);
                    }
                    "bin" => {
                        // Data without a sidecar is unidentifiable -> remove.
                        if !path.with_extension("json").exists() {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    "json" => {
                        let meta: EntryMeta = match std::fs::read(&path)
                            .ok()
                            .and_then(|b| serde_json::from_slice(&b).ok())
                        {
                            Some(m) => m,
                            None => {
                                let _ = std::fs::remove_file(&path);
                                continue;
                            }
                        };
                        let data_path = path.with_extension("bin");
                        match std::fs::metadata(&data_path) {
                            Ok(md) if md.len() == meta.len => {
                                let mtime = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                                found.push((mtime, meta.key, meta.len, data_path, path));
                            }
                            _ => {
                                // Missing or size-mismatched data: treat as corrupt.
                                let _ = std::fs::remove_file(&data_path);
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Oldest mtime first so LRU ordering approximates pre-restart recency.
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut index = Index::default();
        for (_, key, len, data_path, meta_path) in found {
            index.insert(key, len, data_path, meta_path);
        }

        let pins_path = cfg.root.join("pins.json");
        if let Ok(bytes) = std::fs::read(&pins_path) {
            if let Ok(pins) = serde_json::from_slice::<Vec<String>>(&bytes) {
                index.pinned = pins.into_iter().collect();
            }
        }

        Ok(Self {
            cfg,
            index: Mutex::new(index),
            stats: CounterSet::default(),
            tmp_seq: AtomicU64::new(0),
        })
    }

    fn paths_for(&self, key: &str) -> (PathBuf, PathBuf, PathBuf) {
        let hash = hex::encode(Sha256::digest(key.as_bytes()));
        let dir = self.cfg.root.join("objects").join(&hash[..2]);
        let data = dir.join(format!("{hash}.bin"));
        let meta = dir.join(format!("{hash}.json"));
        let n = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{hash}.{n}.tmp"));
        (data, meta, tmp)
    }

    /// Fetch an entry. Returns `None` on miss (including unreadable entries,
    /// which are dropped from the index).
    pub async fn get(&self, key: &str) -> Option<Bytes> {
        let data_path = {
            let mut ix = self.index.lock();
            match ix.entries.get(key) {
                Some(e) => {
                    let p = e.data_path.clone();
                    ix.touch(key);
                    p
                }
                None => {
                    self.stats.miss();
                    return None;
                }
            }
        };
        match tokio::fs::read(&data_path).await {
            Ok(b) => {
                self.stats.hit();
                Some(Bytes::from(b))
            }
            Err(e) => {
                tracing::warn!(key, error = %e, "disk cache entry unreadable; dropping");
                let paths = { self.index.lock().forget(key) };
                if let Some((d, m)) = paths {
                    let _ = tokio::fs::remove_file(d).await;
                    let _ = tokio::fs::remove_file(m).await;
                }
                self.stats.miss();
                None
            }
        }
    }

    /// Insert an entry, evicting LRU entries as needed.
    pub async fn put(&self, key: &str, data: Bytes) -> std::io::Result<()> {
        let (data_path, meta_path, tmp_path) = self.paths_for(key);
        if let Some(parent) = data_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&tmp_path, &data).await?;
        tokio::fs::rename(&tmp_path, &data_path).await?;
        let meta = EntryMeta {
            key: key.to_string(),
            len: data.len() as u64,
        };
        let meta_bytes = serde_json::to_vec(&meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::fs::write(&meta_path, meta_bytes).await?;

        self.stats.inserted(data.len() as u64);
        let victims = {
            let mut ix = self.index.lock();
            ix.insert(key.to_string(), data.len() as u64, data_path, meta_path);
            ix.evict_to_fit(self.cfg.max_bytes)
        };
        if !victims.is_empty() {
            self.stats.evicted(victims.len() as u64);
            for (d, m) in victims {
                let _ = tokio::fs::remove_file(d).await;
                let _ = tokio::fs::remove_file(m).await;
            }
        }
        Ok(())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.index.lock().entries.contains_key(key)
    }

    pub async fn remove(&self, key: &str) -> bool {
        let paths = { self.index.lock().forget(key) };
        match paths {
            Some((d, m)) => {
                let _ = tokio::fs::remove_file(d).await;
                let _ = tokio::fs::remove_file(m).await;
                true
            }
            None => false,
        }
    }

    /// Remove every entry whose key starts with `prefix`. Used when a
    /// namespace is deleted or its segments are superseded by compaction.
    pub async fn remove_prefix(&self, prefix: &str) -> usize {
        let victims: Vec<(PathBuf, PathBuf)> = {
            let mut ix = self.index.lock();
            let keys: Vec<String> = ix
                .entries
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect();
            keys.iter().filter_map(|k| ix.forget(k)).collect()
        };
        let n = victims.len();
        for (d, m) in victims {
            let _ = tokio::fs::remove_file(d).await;
            let _ = tokio::fs::remove_file(m).await;
        }
        n
    }

    /// Pin a key prefix so matching entries are exempt from LRU eviction.
    /// Pins are persisted and survive restarts.
    pub fn pin_prefix(&self, prefix: &str) -> std::io::Result<()> {
        let pins = {
            let mut ix = self.index.lock();
            ix.pinned.insert(prefix.to_string());
            ix.pinned.iter().cloned().collect::<Vec<_>>()
        };
        self.persist_pins(pins)
    }

    pub fn unpin_prefix(&self, prefix: &str) -> std::io::Result<()> {
        let pins = {
            let mut ix = self.index.lock();
            ix.pinned.remove(prefix);
            ix.pinned.iter().cloned().collect::<Vec<_>>()
        };
        self.persist_pins(pins)
    }

    fn persist_pins(&self, mut pins: Vec<String>) -> std::io::Result<()> {
        pins.sort();
        let body = serde_json::to_vec_pretty(&pins)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(self.cfg.root.join("pins.json"), body)
    }

    pub fn total_bytes(&self) -> u64 {
        self.index.lock().total_bytes
    }

    pub fn entry_count(&self) -> usize {
        self.index.lock().entries.len()
    }

    pub fn snapshot(&self) -> DiskTierSnapshot {
        let ix = self.index.lock();
        let mut pins: Vec<String> = ix.pinned.iter().cloned().collect();
        pins.sort();
        DiskTierSnapshot {
            counters: self.stats.snapshot(),
            entries: ix.entries.len(),
            bytes: ix.total_bytes,
            max_bytes: self.cfg.max_bytes,
            pinned_prefixes: pins,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &Path, max: u64) -> DiskCacheConfig {
        DiskCacheConfig {
            root: root.to_path_buf(),
            max_bytes: max,
        }
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
        cache.put("ns/a/seg-1", Bytes::from_static(b"hello")).await.unwrap();
        assert_eq!(cache.get("ns/a/seg-1").await.unwrap(), Bytes::from_static(b"hello"));
        assert!(cache.get("ns/a/seg-2").await.is_none());
        let snap = cache.snapshot();
        assert_eq!(snap.counters.hits, 1);
        assert_eq!(snap.counters.misses, 1);
        assert_eq!(snap.bytes, 5);
    }

    #[tokio::test]
    async fn lru_eviction_respects_recency() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(cfg(dir.path(), 100)).unwrap();
        cache.put("a", Bytes::from(vec![0u8; 40])).await.unwrap();
        cache.put("b", Bytes::from(vec![0u8; 40])).await.unwrap();
        // Touch "a" so "b" becomes least-recently-used.
        assert!(cache.get("a").await.is_some());
        cache.put("c", Bytes::from(vec![0u8; 40])).await.unwrap();
        assert!(cache.get("a").await.is_some());
        assert!(cache.get("b").await.is_none(), "LRU entry should be evicted");
        assert!(cache.get("c").await.is_some());
        assert!(cache.total_bytes() <= 100);
    }

    #[tokio::test]
    async fn pinned_prefix_is_not_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(cfg(dir.path(), 100)).unwrap();
        cache.pin_prefix("pinned/").unwrap();
        cache.put("pinned/x", Bytes::from(vec![0u8; 60])).await.unwrap();
        cache.put("plain/y", Bytes::from(vec![0u8; 60])).await.unwrap();
        // Over budget: the only evictable entry is plain/y, even though
        // pinned/x is older.
        assert!(cache.get("pinned/x").await.is_some());
        assert!(cache.get("plain/y").await.is_none());
    }

    #[tokio::test]
    async fn restart_rebuilds_index_and_pins() {
        let dir = tempfile::tempdir().unwrap();
        {
            let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
            cache.put("ns/a", Bytes::from_static(b"alpha")).await.unwrap();
            cache.put("ns/b", Bytes::from_static(b"beta")).await.unwrap();
            cache.pin_prefix("ns/a").unwrap();
        }
        let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(cache.get("ns/a").await.unwrap(), Bytes::from_static(b"alpha"));
        assert_eq!(cache.get("ns/b").await.unwrap(), Bytes::from_static(b"beta"));
        assert_eq!(cache.snapshot().pinned_prefixes, vec!["ns/a".to_string()]);
    }

    #[tokio::test]
    async fn corrupt_entries_are_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let data_path;
        {
            let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
            cache.put("ns/a", Bytes::from_static(b"alpha")).await.unwrap();
            data_path = {
                let ix = cache.index.lock();
                ix.entries.get("ns/a").unwrap().data_path.clone()
            };
        }
        // Truncate the data file: size no longer matches the sidecar.
        std::fs::write(&data_path, b"a").unwrap();
        let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.get("ns/a").await.is_none());
    }

    #[tokio::test]
    async fn remove_prefix_clears_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DiskCache::open(cfg(dir.path(), 1 << 20)).unwrap();
        cache.put("ns/a/1", Bytes::from_static(b"1")).await.unwrap();
        cache.put("ns/a/2", Bytes::from_static(b"2")).await.unwrap();
        cache.put("ns/b/1", Bytes::from_static(b"3")).await.unwrap();
        assert_eq!(cache.remove_prefix("ns/a/").await, 2);
        assert!(cache.get("ns/a/1").await.is_none());
        assert!(cache.get("ns/b/1").await.is_some());
    }
}
