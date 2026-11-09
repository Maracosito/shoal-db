//! Cache warming and namespace pinning.
//!
//! Warming pre-loads every immutable object referenced by a namespace's
//! manifest through the cache hierarchy (populating the SSD tier) and
//! optionally materializes the in-memory state, so the next query is a warm
//! query.
//!
//! Pinning marks a namespace as exempt from cache eviction at both tiers and
//! persists the pin set in object storage (`_meta/pins.json` at the bucket
//! root) so pinned namespaces are re-warmed automatically after a restart.

use std::collections::BTreeSet;
use std::sync::RwLock;
use std::time::Instant;

use bytes::Bytes;
use futures::stream::{self, StreamExt};
use serde::Serialize;
use tokio::sync::Mutex;

use super::{storage_err, Engine, NsPath, Result, Store};

/// Concurrency for warm-up object fetches.
const WARM_FETCH_CONCURRENCY: usize = 8;

/// Where the pin set is persisted.
const PINS_KEY: &str = "_meta/pins.json";

#[derive(Debug, Serialize)]
pub struct WarmReport {
    pub namespace: String,
    /// Number of immutable objects referenced by the manifest.
    pub objects: usize,
    /// Total bytes pulled through the cache hierarchy.
    pub bytes: u64,
    /// Whether the in-memory state was (re)built.
    pub state_built: bool,
    pub took_ms: u64,
    pub pinned: bool,
}

impl Engine {
    /// Pre-load a namespace's objects into the cache hierarchy. With
    /// `build_state == true`, also materialize the in-memory state.
    pub async fn warm_namespace(&self, path: &NsPath, build_state: bool) -> Result<WarmReport> {
        let started = Instant::now();
        let manifest = self.load_manifest(path).await?;
        let keys = manifest.object_keys();
        let objects = keys.len();

        let fetched: Vec<u64> = stream::iter(keys)
            .map(|key| {
                let cache = self.cache.clone();
                async move {
                    match cache.get(&key).await {
                        Ok(bytes) => bytes.len() as u64,
                        Err(e) => {
                            tracing::warn!(key = %key, error = %e, "warm fetch failed");
                            0
                        }
                    }
                }
            })
            .buffer_unordered(WARM_FETCH_CONCURRENCY)
            .collect()
            .await;
        let bytes: u64 = fetched.iter().sum();

        if build_state {
            self.namespace_state(path).await?;
        }

        Ok(WarmReport {
            namespace: path.display(),
            objects,
            bytes,
            state_built: build_state,
            took_ms: started.elapsed().as_millis() as u64,
            pinned: self.pins.contains(&path.display()),
        })
    }

    /// Pin a namespace: exempt it from cache eviction (memory and disk
    /// tiers), persist the pin durably, and warm it immediately.
    pub async fn pin_namespace(&self, path: &NsPath) -> Result<WarmReport> {
        // Verify the namespace exists before pinning.
        self.load_manifest(path).await?;
        self.pins.pin(&path.display()).await?;
        self.cache.pin_prefix(&path.root());
        self.states.set_pinned(&path.root(), true);
        let mut report = self.warm_namespace(path, true).await?;
        report.pinned = true;
        Ok(report)
    }

    /// Remove a pin. Returns whether the namespace was pinned.
    pub async fn unpin_namespace(&self, path: &NsPath) -> Result<bool> {
        let was_pinned = self.pins.unpin(&path.display()).await?;
        if was_pinned {
            self.cache.unpin_prefix(&path.root());
            self.states.set_pinned(&path.root(), false);
        }
        Ok(was_pinned)
    }

    pub fn pinned_namespaces(&self) -> Vec<String> {
        self.pins.all()
    }
}

/// Durable set of pinned namespaces.
pub struct PinSet {
    store: Store,
    set: RwLock<BTreeSet<String>>,
    /// Serializes persistence so concurrent pin/unpin cannot interleave
    /// stale snapshots.
    io: Mutex<()>,
}

impl PinSet {
    pub fn new(store: Store) -> Self {
        PinSet {
            store,
            set: RwLock::new(BTreeSet::new()),
            io: Mutex::new(()),
        }
    }

    /// Load the persisted pin set (called once on startup).
    pub async fn load(&self) -> Result<()> {
        if !self.store.exists(PINS_KEY).await.map_err(storage_err)? {
            return Ok(());
        }
        let bytes = self.store.get(PINS_KEY).await.map_err(storage_err)?;
        let pins: Vec<String> = serde_json::from_slice(&bytes).map_err(super::core_err)?;
        let mut set = self.set.write().expect("pin set lock poisoned");
        set.clear();
        set.extend(pins);
        Ok(())
    }

    pub fn contains(&self, ns_path: &str) -> bool {
        self.set
            .read()
            .expect("pin set lock poisoned")
            .contains(ns_path)
    }

    pub fn all(&self) -> Vec<String> {
        self.set
            .read()
            .expect("pin set lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub async fn pin(&self, ns_path: &str) -> Result<()> {
        let _io = self.io.lock().await;
        {
            let mut set = self.set.write().expect("pin set lock poisoned");
            set.insert(ns_path.to_string());
        }
        self.persist().await
    }

    /// Returns whether the entry existed.
    pub async fn unpin(&self, ns_path: &str) -> Result<bool> {
        let _io = self.io.lock().await;
        let removed = {
            let mut set = self.set.write().expect("pin set lock poisoned");
            set.remove(ns_path)
        };
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    async fn persist(&self) -> Result<()> {
        let snapshot: Vec<String> = {
            self.set
                .read()
                .expect("pin set lock poisoned")
                .iter()
                .cloned()
                .collect()
        };
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(super::core_err)?;
        self.store
            .put(PINS_KEY, Bytes::from(bytes))
            .await
            .map_err(storage_err)
    }
}
