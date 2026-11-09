//! Background indexer: folds the memtable (i.e. unfolded WAL records) into
//! immutable level-0 segments and truncates the WAL.
//!
//! ## Fold protocol & crash safety
//!
//! ```text
//!  step                                    crash here leaves...
//!  1. snapshot memtable @ watermark        nothing (pure read)
//!  2. write segment object (if-absent)     orphan segment -> GC deletes it
//!  3. commit manifest v+1                  commit IS the atomic point:
//!     (last_applied_lsn = watermark,        - before: state unchanged
//!      segments += new)                     - after: segment is live
//!  4. persist idempotency ledger           WAL still present -> recovery
//!                                          re-harvests keys from WAL
//!  5. delete WAL records <= watermark      leftover folded WAL records are
//!                                          skipped by recovery, GC deletes
//! ```
//!
//! Every failpoint name is suffixed with `/<namespace>` so tests can inject
//! crashes into one namespace without affecting concurrently running tests.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use reef_store::failpoint;
use reef_types::DocId;

use crate::engine::Engine;
use crate::error::{EngineError, EngineResult};
use crate::layout;
use crate::manifest;
use crate::memtable::DocState;
use crate::now_ms;
use crate::segment::{self, SegmentBuilder};
use crate::wal::Wal;

/// Result of a successful fold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldOutcome {
    pub namespace: String,
    pub segment_id: String,
    /// Entries written into the segment (live docs + tombstones).
    pub docs: u64,
    /// All WAL records with `lsn <= watermark_lsn` are now represented by
    /// segments.
    pub watermark_lsn: u64,
    pub manifest_version: u64,
    pub wal_records_deleted: usize,
}

/// Fold the namespace's memtable into one level-0 segment. Returns `None`
/// when there is nothing to fold. Serialized against compaction/GC via the
/// per-namespace maintenance mutex.
pub async fn fold_namespace(engine: &Engine, ns: &str) -> EngineResult<Option<FoldOutcome>> {
    let h = engine.handle(ns).await?;
    let _maintenance = h.maintenance.lock().await;

    // 1. Snapshot.
    let (snapshot, watermark, base_manifest): (Vec<(DocId, u64, DocState)>, u64, _) = {
        let inner = h.inner.read().await;
        if inner.memtable.len() == 0 {
            return Ok(None);
        }
        let watermark = inner.memtable.max_lsn();
        let snap = inner
            .memtable
            .iter()
            .filter(|(_, e)| e.lsn <= watermark)
            .map(|(id, e)| (id.clone(), e.lsn, e.state.clone()))
            .collect();
        (snap, watermark, inner.manifest.clone())
    };

    // 2. Build + write the segment object.
    let segment_id = segment::new_segment_id();
    let mut builder = SegmentBuilder::new(&segment_id, 0);
    for (id, lsn, state) in &snapshot {
        builder.add(id.clone(), *lsn, state.clone());
    }
    let (bytes, meta) = builder.finish()?;
    let entry_count = snapshot.len() as u64;

    failpoint::maybe_fail(&format!("indexer.before_segment_put/{ns}"))?;
    let created = engine
        .store_ref()
        .put_if_absent(&layout::segment_key(ns, &segment_id), bytes)
        .await?;
    if !created {
        return Err(EngineError::Conflict(format!(
            "segment id collision for '{segment_id}' in namespace '{ns}'"
        )));
    }

    // 3. Atomic commit: manifest v+1.
    failpoint::maybe_fail(&format!("indexer.before_manifest_commit/{ns}"))?;
    let mut new_manifest = base_manifest.clone();
    new_manifest.version += 1;
    new_manifest.updated_at_ms = now_ms();
    new_manifest.last_applied_lsn = watermark;
    new_manifest.segments.push(meta);
    manifest::commit(engine.store_ref(), &new_manifest).await?;
    failpoint::maybe_fail(&format!("indexer.after_manifest_commit/{ns}"))?;

    // 4. Swap in-memory state and persist the idempotency ledger. Entries
    //    written after the snapshot (lsn > watermark) survive in the memtable.
    let ledger_snapshot = {
        let mut inner = h.inner.write().await;
        inner.manifest = new_manifest.clone();
        inner.memtable.clear_upto(watermark);
        engine.prune_idempotency(&mut inner.idempotency);
        inner.idempotency.clone()
    };
    engine.save_ledger(ns, &ledger_snapshot).await?;

    // 5. Truncate the WAL.
    failpoint::maybe_fail(&format!("indexer.before_wal_delete/{ns}"))?;
    let wal = Wal::new(engine.store_ref().clone(), ns);
    let wal_records_deleted = wal.delete_upto(watermark).await?;

    info!(
        namespace = ns,
        segment_id = %segment_id,
        docs = entry_count,
        watermark_lsn = watermark,
        manifest_version = new_manifest.version,
        wal_records_deleted,
        "folded WAL into level-0 segment"
    );

    Ok(Some(FoldOutcome {
        namespace: ns.to_string(),
        segment_id,
        docs: entry_count,
        watermark_lsn: watermark,
        manifest_version: new_manifest.version,
        wal_records_deleted,
    }))
}

// ---------------------------------------------------------------------------
// Maintenance loop
// ---------------------------------------------------------------------------

/// Handle to a running background maintenance task.
pub struct MaintenanceHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl MaintenanceHandle {
    /// Signal shutdown and wait for the loop to finish its current pass.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Spawn the background maintenance loop: per tick it folds due memtables,
/// runs (trigger-gated) compaction, and periodically garbage-collects. Only
/// namespaces opened in this process are maintained; a separate worker can
/// iterate `Engine::list_namespaces` to maintain everything.
pub fn spawn_maintenance(engine: Arc<Engine>) -> MaintenanceHandle {
    let (tx, rx) = watch::channel(false);
    let task = tokio::spawn(maintenance_loop(engine, rx));
    MaintenanceHandle { shutdown: tx, task }
}

async fn maintenance_loop(engine: Arc<Engine>, mut shutdown: watch::Receiver<bool>) {
    let opts = engine.options().clone();
    let mut tick: u64 = 0;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(opts.maintenance_interval_ms)) => {}
        }
        tick += 1;

        for ns in engine.open_namespaces().await {
            // Fold decision.
            if let Some((entries, bytes)) = engine.maintenance_snapshot(&ns).await {
                let urgent = entries >= opts.fold_max_memtable_entries
                    || bytes >= opts.fold_max_memtable_bytes;
                let cadence_due =
                    entries > 0 && tick % u64::from(opts.fold_every_n_ticks.max(1)) == 0;
                if urgent || cadence_due {
                    match engine.fold_now(&ns).await {
                        Ok(Some(o)) => debug!(
                            namespace = %ns,
                            segment_id = %o.segment_id,
                            docs = o.docs,
                            "maintenance fold complete"
                        ),
                        Ok(None) => {}
                        Err(e) => warn!(namespace = %ns, error = %e, "maintenance fold failed"),
                    }
                }
            }

            // Compaction is self-gated by the level-0 trigger.
            match engine.compact_now(&ns, false).await {
                Ok(Some(o)) => debug!(
                    namespace = %ns,
                    inputs = o.inputs.len(),
                    outputs = o.outputs.len(),
                    "maintenance compaction complete"
                ),
                Ok(None) => {}
                Err(e) => warn!(namespace = %ns, error = %e, "maintenance compaction failed"),
            }

            // Periodic GC.
            if tick % u64::from(opts.gc_every_n_ticks.max(1)) == 0 {
                if let Err(e) = engine.gc_now(&ns).await {
                    warn!(namespace = %ns, error = %e, "maintenance gc failed");
                }
            }
        }
    }
    debug!("maintenance loop stopped");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::WriteRequest;
    use crate::options::EngineOptions;
    use reef_store::memory::MemoryStore;
    use reef_store::ObjectStore;
    use reef_types::{Document, Value};
    use std::collections::BTreeMap;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    fn engine(store: &Arc<dyn ObjectStore>) -> Engine {
        Engine::new(store.clone(), EngineOptions::for_tests())
    }

    fn doc(id: &str, n: i64) -> Document {
        let mut attributes = BTreeMap::new();
        attributes.insert("n".to_string(), Value::I64(n));
        Document {
            id: DocId::from(id.to_string()),
            vector: None,
            sparse_vector: None,
            attributes,
        }
    }

    #[tokio::test]
    async fn crash_before_fold_manifest_commit_is_recoverable() {
        let ns = "fold_crash_pre_commit";
        let fp = format!("indexer.before_manifest_commit/{ns}");
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2), doc("c", 3)]))
            .await
            .unwrap();

        // Crash after the segment object is written, before the manifest
        // commit: the segment is an orphan and the fold never happened.
        failpoint::arm(&fp);
        assert!(e.fold_now(ns).await.is_err());
        failpoint::disarm(&fp);

        // Simulate process restart on the same store.
        let e2 = engine(&store);
        let stats = e2.stats(ns).await.unwrap();
        assert_eq!(stats.segment_count, 0, "manifest must be unchanged");
        assert_eq!(e2.export(ns).await.unwrap().len(), 3, "data served from WAL");

        // A retried fold succeeds and GC removes the orphaned segment.
        let outcome = e2.fold_now(ns).await.unwrap().unwrap();
        assert_eq!(outcome.docs, 3);
        let gc = e2.gc_now(ns).await.unwrap();
        assert_eq!(gc.deleted_segments.len(), 1, "orphan from crashed fold collected");
        assert_eq!(e2.export(ns).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn crash_after_fold_manifest_commit_before_wal_truncation() {
        let ns = "fold_crash_post_commit";
        let fp = format!("indexer.after_manifest_commit/{ns}");
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();

        // Crash after the manifest commit: the fold IS durable, but WAL
        // records and the in-memory swap never happened.
        failpoint::arm(&fp);
        assert!(e.fold_now(ns).await.is_err());
        failpoint::disarm(&fp);

        // Restart: recovery must see the new manifest, skip the already
        // folded WAL records, and serve identical data.
        let e2 = engine(&store);
        let stats = e2.stats(ns).await.unwrap();
        assert_eq!(stats.segment_count, 1);
        assert_eq!(stats.pending_memtable_entries, 0, "folded WAL records skipped at replay");
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);

        // GC removes the leftover folded WAL records.
        let gc = e2.gc_now(ns).await.unwrap();
        assert!(gc.deleted_wal >= 1, "leftover folded WAL records collected");

        // Writes keep working at the next LSN.
        let r = e2.write(ns, WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();
        assert_eq!(r.lsn, 2);
        assert_eq!(e2.export(ns).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn maintenance_loop_folds_automatically() {
        let ns = "auto_fold";
        let store = mem();
        let e = Arc::new(engine(&store));
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();

        let handle = spawn_maintenance(e.clone());
        // for_tests(): 25ms ticks, fold_every_n_ticks=1 — a few ticks suffice.
        let mut folded = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if e.stats(ns).await.unwrap().pending_memtable_entries == 0 {
                folded = true;
                break;
            }
        }
        handle.shutdown().await;
        assert!(folded, "maintenance loop should fold the memtable");
        assert!(e.stats(ns).await.unwrap().segment_count >= 1);
        assert_eq!(e.export(ns).await.unwrap().len(), 2);
    }
}
