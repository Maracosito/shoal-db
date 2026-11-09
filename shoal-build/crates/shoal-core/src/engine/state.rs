//! Materialized namespace state and its in-memory LRU cache.
//!
//! A `NamespaceState` is the fully resolved view of a namespace at one
//! manifest version: segments applied oldest-to-newest, then WAL batches in
//! sequence order, last-writer-wins per document id. States are immutable
//! and shared via `Arc`; queries never block writers.
//!
//! The `StateCache` is the in-memory tier of the cache hierarchy for
//! query-serving data. The disk tier (raw immutable objects) lives in
//! `shoal-cache`; this layer avoids re-decoding segments/WAL on every query.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use std::sync::Arc;

use crate::manifest::Manifest;
use crate::segment;
use crate::types::{AttributeValue, Document};
use crate::wal::{WalBatch, WalRecord};

use super::{core_err, storage_err, Engine, Result};

/// Fully resolved, immutable namespace state at a specific manifest version.
pub struct NamespaceState {
    pub version: u64,
    pub docs: Vec<Document>,
    by_id: HashMap<String, usize>,
    pub approx_bytes: usize,
}

impl NamespaceState {
    pub fn new(version: u64, docs: Vec<Document>) -> Self {
        let mut by_id = HashMap::with_capacity(docs.len());
        let mut approx_bytes = 0usize;
        for (i, d) in docs.iter().enumerate() {
            by_id.insert(d.id.clone(), i);
            approx_bytes += estimate_doc_bytes(d);
        }
        NamespaceState {
            version,
            docs,
            by_id,
            approx_bytes,
        }
    }

    pub fn get(&self, id: &str) -> Option<&Document> {
        self.by_id.get(id).map(|&i| &self.docs[i])
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

fn estimate_doc_bytes(d: &Document) -> usize {
    let mut n = 64 + d.id.len();
    if let Some(v) = &d.vector {
        n += v.len() * 4;
    }
    if let Some(sv) = &d.sparse_vector {
        n += sv.indices.len() * 4 + sv.values.len() * 4;
    }
    for (k, v) in &d.text {
        n += k.len() + v.len() + 16;
    }
    for (k, v) in &d.attributes {
        n += k.len() + estimate_attr_bytes(v) + 16;
    }
    n
}

fn estimate_attr_bytes(v: &AttributeValue) -> usize {
    match v {
        AttributeValue::Null => 4,
        AttributeValue::Bool(_) => 5,
        AttributeValue::Int(_) | AttributeValue::Float(_) => 12,
        AttributeValue::String(s) => s.len() + 2,
        AttributeValue::StringList(a) => 2 + a.iter().map(|s| s.len() + 2).sum::<usize>(),
    }
}

/// Build the live document set for a manifest by reading all referenced
/// immutable objects through the cache hierarchy and applying them in order.
pub async fn materialize(engine: &Engine, manifest: &Manifest) -> Result<Vec<Document>> {
    let mut map: BTreeMap<String, Document> = BTreeMap::new();
    for seg in &manifest.segments {
        let bytes = engine.cache.get(&seg.key).await.map_err(storage_err)?;
        let docs = segment::read_segment(&bytes).map_err(core_err)?;
        for d in docs {
            map.insert(d.id.clone(), d);
        }
    }
    for wal in &manifest.wal {
        let bytes = engine.cache.get(&wal.key).await.map_err(storage_err)?;
        let batch = WalBatch::decode(&bytes).map_err(core_err)?;
        for record in batch.records {
            match record {
                WalRecord::Upsert(d) => {
                    map.insert(d.id.clone(), d);
                }
                WalRecord::Delete { id } => {
                    map.remove(&id);
                }
            }
        }
    }
    Ok(map.into_values().collect())
}

// ---------------------------------------------------------------------------
// LRU cache of materialized states
// ---------------------------------------------------------------------------

struct Entry {
    state: Arc<NamespaceState>,
    pinned: bool,
    last_used: AtomicU64,
}

/// LRU cache keyed by namespace root, validated by manifest version.
/// Pinned namespaces are exempt from eviction (unless everything is pinned,
/// in which case the cache may exceed its budget; pinning is an explicit
/// operator decision).
pub struct StateCache {
    max_entries: usize,
    max_bytes: usize,
    tick: AtomicU64,
    map: RwLock<HashMap<String, Entry>>,
}

impl StateCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        StateCache {
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            tick: AtomicU64::new(0),
            map: RwLock::new(HashMap::new()),
        }
    }

    fn next_tick(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Get the cached state if present *and* current for `version`.
    pub fn get(&self, root: &str, version: u64) -> Option<Arc<NamespaceState>> {
        let map = self.map.read().expect("state cache lock poisoned");
        let entry = map.get(root)?;
        if entry.state.version != version {
            return None;
        }
        entry.last_used.store(self.next_tick(), Ordering::Relaxed);
        Some(entry.state.clone())
    }

    pub fn put(&self, root: &str, state: Arc<NamespaceState>, pinned: bool) {
        let mut map = self.map.write().expect("state cache lock poisoned");
        map.insert(
            root.to_string(),
            Entry {
                state,
                pinned,
                last_used: AtomicU64::new(self.next_tick()),
            },
        );
        // Evict while over budget, never evicting pinned entries or the
        // entry we just inserted (it is the most recently used by definition).
        loop {
            let total_bytes: usize = map.values().map(|e| e.state.approx_bytes).sum();
            if map.len() <= self.max_entries && total_bytes <= self.max_bytes {
                break;
            }
            let victim = map
                .iter()
                .filter(|(k, e)| !e.pinned && k.as_str() != root)
                .min_by_key(|(_, e)| e.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    map.remove(&k);
                }
                None => break, // only pinned entries (or just this one) remain
            }
        }
    }

    pub fn invalidate(&self, root: &str) {
        self.map
            .write()
            .expect("state cache lock poisoned")
            .remove(root);
    }

    /// Update the pinned flag of a cached entry, if present.
    pub fn set_pinned(&self, root: &str, pinned: bool) {
        if let Some(e) = self
            .map
            .write()
            .expect("state cache lock poisoned")
            .get_mut(root)
        {
            e.pinned = pinned;
        }
    }

    pub fn entries(&self) -> usize {
        self.map.read().expect("state cache lock poisoned").len()
    }

    pub fn bytes(&self) -> usize {
        self.map
            .read()
            .expect("state cache lock poisoned")
            .values()
            .map(|e| e.state.approx_bytes)
            .sum()
    }
}
