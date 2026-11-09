//! Reference counting for immutable objects shared between namespaces.
//!
//! Copy-on-write branching makes multiple namespace manifests reference the
//! same immutable segment/WAL objects. An object may only be deleted when
//! *no* manifest references it. We track this in a per-project refcount
//! table, stored as a single JSON object at
//! `{project_root}/_meta/refcounts.json`.
//!
//! Every manifest reference counts as one: the owning namespace's own
//! reference is included, so "refcount == 0" means "safe to delete",
//! unconditionally.
//!
//! # Concurrency
//!
//! Table updates are read-modify-write cycles guarded by an in-process
//! per-project mutex, consistent with the single-writer-per-deployment
//! consistency model documented for v1. Multi-node deployments must route
//! metadata mutations for a project to one node (or accept that refcounts
//! may drift and rely on `repair`).
//!
//! # Safety bias
//!
//! Decrementing a key that is missing from the table logs a warning and does
//! NOT report the key as deletable: when in doubt, we leak rather than
//! delete something that might still be referenced. `rebuild` reconstructs
//! the table exactly from live manifests.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use shoal_core::layout;
use shoal_core::manifest::Manifest;

use super::{join_key, storage_err, Result, Store};

/// The persisted refcount table for one project.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RefTable {
    /// Map from absolute object key to number of manifests referencing it.
    pub counts: BTreeMap<String, u64>,
}

pub struct RefCounter {
    store: Store,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

fn table_key(org: &str, project: &str) -> String {
    join_key(&layout::project_root(org, project), "_meta/refcounts.json")
}

impl RefCounter {
    pub fn new(store: Store) -> Self {
        RefCounter {
            store,
            locks: Mutex::new(HashMap::new()),
        }
    }

    async fn project_lock(&self, org: &str, project: &str) -> Arc<Mutex<()>> {
        let key = format!("{org}/{project}");
        self.locks
            .lock()
            .await
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn load_table(&self, key: &str) -> Result<RefTable> {
        if !self.store.exists(key).await.map_err(storage_err)? {
            return Ok(RefTable::default());
        }
        let bytes = self.store.get(key).await.map_err(storage_err)?;
        serde_json::from_slice(&bytes).map_err(super::core_err)
    }

    async fn store_table(&self, key: &str, table: &RefTable) -> Result<()> {
        let bytes = serde_json::to_vec(table).map_err(super::core_err)?;
        self.store
            .put(key, Bytes::from(bytes))
            .await
            .map_err(storage_err)
    }

    /// Increment the refcount of each key by one.
    pub async fn incr(&self, org: &str, project: &str, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let lock = self.project_lock(org, project).await;
        let _guard = lock.lock().await;
        let tkey = table_key(org, project);
        let mut table = self.load_table(&tkey).await?;
        for key in keys {
            *table.counts.entry(key.clone()).or_insert(0) += 1;
        }
        self.store_table(&tkey, &table).await
    }

    /// Decrement the refcount of each key by one. Returns the keys whose
    /// count reached zero (and which were removed from the table); these are
    /// safe to delete from object storage.
    pub async fn decr(&self, org: &str, project: &str, keys: &[String]) -> Result<Vec<String>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let lock = self.project_lock(org, project).await;
        let _guard = lock.lock().await;
        let tkey = table_key(org, project);
        let mut table = self.load_table(&tkey).await?;
        let mut zeroed = Vec::new();
        for key in keys {
            match table.counts.get_mut(key) {
                Some(count) if *count > 1 => {
                    *count -= 1;
                }
                Some(_) => {
                    table.counts.remove(key);
                    zeroed.push(key.clone());
                }
                None => {
                    // Conservative: never report an untracked key as deletable.
                    tracing::warn!(
                        key = %key,
                        "refcount decrement for untracked object; leaking (run repair)"
                    );
                }
            }
        }
        self.store_table(&tkey, &table).await?;
        Ok(zeroed)
    }

    /// Current refcount of a key (0 if untracked).
    pub async fn get(&self, org: &str, project: &str, key: &str) -> Result<u64> {
        let lock = self.project_lock(org, project).await;
        let _guard = lock.lock().await;
        let table = self.load_table(&table_key(org, project)).await?;
        Ok(table.counts.get(key).copied().unwrap_or(0))
    }

    /// Rebuild the table exactly from the given live manifests and persist it.
    pub async fn rebuild<'a>(
        &self,
        org: &str,
        project: &str,
        manifests: impl Iterator<Item = &'a Manifest>,
    ) -> Result<RefTable> {
        let lock = self.project_lock(org, project).await;
        let _guard = lock.lock().await;
        let mut table = RefTable::default();
        for manifest in manifests {
            for key in manifest.object_keys() {
                *table.counts.entry(key).or_insert(0) += 1;
            }
        }
        self.store_table(&table_key(org, project), &table).await?;
        Ok(table)
    }
}
