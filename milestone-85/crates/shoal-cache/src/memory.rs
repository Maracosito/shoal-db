//! In-memory hot cache tier.
//!
//! A weight-bounded LRU keyed by string (normally an object-storage key or a
//! derived key like `manifest:<ns>`), storing `Arc<V>` so decoded values
//! (manifests, index headers) can be shared without copying. The weigher is
//! supplied at construction so callers decide how to cost entries (byte length
//! for raw bodies, estimated heap size for decoded structures).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::stats::{CounterSet, CounterSnapshot};

struct MemEntry<V> {
    value: Arc<V>,
    weight: u64,
    tick: u64,
}

struct MemIndex<V> {
    entries: HashMap<String, MemEntry<V>>,
    order: BTreeMap<u64, String>,
    next_tick: u64,
    total_weight: u64,
    pinned: HashSet<String>,
}

impl<V> Default for MemIndex<V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            next_tick: 0,
            total_weight: 0,
            pinned: HashSet::new(),
        }
    }
}

impl<V> MemIndex<V> {
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

    fn forget(&mut self, key: &str) -> bool {
        if let Some(e) = self.entries.remove(key) {
            self.order.remove(&e.tick);
            self.total_weight = self.total_weight.saturating_sub(e.weight);
            true
        } else {
            false
        }
    }

    fn evict_to_fit(&mut self, max_weight: u64) -> u64 {
        let mut evicted = 0;
        while self.total_weight > max_weight {
            let candidate = self
                .order
                .iter()
                .find(|(_, k)| !self.is_pinned(k))
                .map(|(_, k)| k.clone());
            match candidate {
                Some(key) => {
                    if self.forget(&key) {
                        evicted += 1;
                    }
                }
                None => break,
            }
        }
        evicted
    }
}

pub struct MemoryCache<V: Send + Sync + 'static> {
    max_weight: u64,
    weigher: Box<dyn Fn(&V) -> u64 + Send + Sync>,
    inner: Mutex<MemIndex<V>>,
    stats: CounterSet,
}

impl<V: Send + Sync + 'static> MemoryCache<V> {
    pub fn new(max_weight: u64, weigher: impl Fn(&V) -> u64 + Send + Sync + 'static) -> Self {
        Self {
            max_weight,
            weigher: Box::new(weigher),
            inner: Mutex::new(MemIndex::default()),
            stats: CounterSet::default(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Arc<V>> {
        let mut ix = self.inner.lock();
        if ix.entries.contains_key(key) {
            ix.touch(key);
            self.stats.hit();
            ix.entries.get(key).map(|e| Arc::clone(&e.value))
        } else {
            self.stats.miss();
            None
        }
    }

    /// Insert a value. Values heavier than the whole cache budget are returned
    /// without being cached. Returns the (possibly shared) `Arc`.
    pub fn put(&self, key: &str, value: V) -> Arc<V> {
        let weight = (self.weigher)(&value);
        let arc = Arc::new(value);
        if weight > self.max_weight {
            return arc;
        }
        self.stats.inserted(weight);
        let evicted = {
            let mut ix = self.inner.lock();
            ix.forget(key);
            ix.next_tick += 1;
            let tick = ix.next_tick;
            ix.order.insert(tick, key.to_string());
            ix.entries.insert(
                key.to_string(),
                MemEntry {
                    value: Arc::clone(&arc),
                    weight,
                    tick,
                },
            );
            ix.total_weight += weight;
            ix.evict_to_fit(self.max_weight)
        };
        if evicted > 0 {
            self.stats.evicted(evicted);
        }
        arc
    }

    pub fn remove(&self, key: &str) -> bool {
        self.inner.lock().forget(key)
    }

    pub fn remove_prefix(&self, prefix: &str) -> usize {
        let mut ix = self.inner.lock();
        let keys: Vec<String> = ix
            .entries
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        let mut n = 0;
        for k in &keys {
            if ix.forget(k) {
                n += 1;
            }
        }
        n
    }

    pub fn pin_prefix(&self, prefix: &str) {
        self.inner.lock().pinned.insert(prefix.to_string());
    }

    pub fn unpin_prefix(&self, prefix: &str) {
        self.inner.lock().pinned.remove(prefix);
    }

    pub fn entries(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn total_weight(&self) -> u64 {
        self.inner.lock().total_weight
    }

    pub fn snapshot_counters(&self) -> CounterSnapshot {
        self.stats.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_cache(max: u64) -> MemoryCache<Vec<u8>> {
        MemoryCache::new(max, |v: &Vec<u8>| v.len() as u64)
    }

    #[test]
    fn put_get_and_weight_accounting() {
        let c = byte_cache(100);
        c.put("a", vec![0u8; 30]);
        c.put("b", vec![0u8; 30]);
        assert_eq!(c.entries(), 2);
        assert_eq!(c.total_weight(), 60);
        assert_eq!(c.get("a").unwrap().len(), 30);
        assert!(c.get("missing").is_none());
        let s = c.snapshot_counters();
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 1);
    }

    #[test]
    fn lru_eviction_order() {
        let c = byte_cache(100);
        c.put("a", vec![0u8; 40]);
        c.put("b", vec![0u8; 40]);
        assert!(c.get("a").is_some()); // a becomes most recent
        c.put("c", vec![0u8; 40]); // forces eviction of b
        assert!(c.get("a").is_some());
        assert!(c.get("b").is_none());
        assert!(c.get("c").is_some());
        assert!(c.total_weight() <= 100);
    }

    #[test]
    fn pinned_entries_survive_pressure() {
        let c = byte_cache(100);
        c.pin_prefix("hot/");
        c.put("hot/a", vec![0u8; 60]);
        c.put("cold/b", vec![0u8; 60]);
        assert!(c.get("hot/a").is_some());
        assert!(c.get("cold/b").is_none());
    }

    #[test]
    fn oversized_value_is_not_cached() {
        let c = byte_cache(10);
        let arc = c.put("big", vec![0u8; 100]);
        assert_eq!(arc.len(), 100);
        assert_eq!(c.entries(), 0);
    }

    #[test]
    fn replace_updates_weight() {
        let c = byte_cache(100);
        c.put("a", vec![0u8; 80]);
        c.put("a", vec![0u8; 20]);
        assert_eq!(c.total_weight(), 20);
        assert_eq!(c.entries(), 1);
    }
}
