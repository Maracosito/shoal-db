//! Lock-free counters for cache tiers, plus serializable snapshots used by the
//! `/v1/cache/stats` endpoint and Prometheus exporters.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Hit/miss/eviction counters shared by both cache tiers.
#[derive(Debug, Default)]
pub struct CounterSet {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    inserted_bytes: AtomicU64,
}

impl CounterSet {
    pub fn hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn evicted(&self, n: u64) {
        self.evictions.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inserted(&self, bytes: u64) {
        self.inserted_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            inserted_bytes: self.inserted_bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CounterSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub inserted_bytes: u64,
}

/// Which-tier-served-the-request counters for the composed [`crate::CacheLayer`].
#[derive(Debug, Default)]
pub struct TierStats {
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
    origin_fetches: AtomicU64,
    origin_bytes: AtomicU64,
}

impl TierStats {
    pub fn memory_hit(&self) {
        self.memory_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn disk_hit(&self) {
        self.disk_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn origin_fetch(&self, bytes: u64) {
        self.origin_fetches.fetch_add(1, Ordering::Relaxed);
        self.origin_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> TierSnapshot {
        TierSnapshot {
            memory_hits: self.memory_hits.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            origin_fetches: self.origin_fetches.load(Ordering::Relaxed),
            origin_bytes: self.origin_bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierSnapshot {
    pub memory_hits: u64,
    pub disk_hits: u64,
    pub origin_fetches: u64,
    pub origin_bytes: u64,
}

/// Snapshot of the disk tier, including capacity usage and persisted pins.
#[derive(Debug, Clone, Serialize)]
pub struct DiskTierSnapshot {
    pub counters: CounterSnapshot,
    pub entries: usize,
    pub bytes: u64,
    pub max_bytes: u64,
    pub pinned_prefixes: Vec<String>,
}

/// Snapshot of the full composed cache layer.
#[derive(Debug, Clone, Serialize)]
pub struct LayerSnapshot {
    pub tiers: TierSnapshot,
    pub memory: CounterSnapshot,
    pub memory_entries: usize,
    pub memory_bytes: u64,
    pub disk: Option<DiskTierSnapshot>,
}
