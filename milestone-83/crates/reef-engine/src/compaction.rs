//! Compaction and garbage collection.
//!
//! ## Compaction policy (v0.2)
//!
//! Reef currently performs **full merges**: when the level-0 segment count
//! reaches `l0_compact_trigger` (or when forced), *all* segments are merged
//! into one or more level-1 segments split at
//! `compaction_target_segment_bytes`. Because the merge always covers every
//! segment, tombstones can be dropped unconditionally — there is no older
//! segment left for them to mask. A leveled/partial policy that avoids
//! rewriting large L1 segments on every trigger is on the roadmap and the
//! protocol below already supports it.
//!
//! ## Crash safety
//!
//! ```text
//!  step                                  crash here leaves...
//!  1. read inputs, merge in memory       nothing
//!  2. write output segments (if-absent)  orphan outputs -> GC deletes
//!  3. commit manifest v+1                atomic point (see indexer.rs)
//!  4. swap in-memory manifest            -
//!  5. delete input segment objects       orphan inputs -> GC deletes
//! ```
//!
//! GC only deletes objects that are (a) unreferenced by the *current*
//! manifest and (b) older than `gc_grace_ms`, so freshly written outputs of
//! a concurrent fold/compaction in another process are never collected
//! prematurely.

use std::collections::{BTreeMap, HashSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use reef_store::failpoint;
use reef_types::DocId;

use crate::engine::Engine;
use crate::error::{EngineError, EngineResult};
use crate::layout;
use crate::manifest;
use crate::memtable::DocState;
use crate::now_ms;
use crate::segment::{self, SegmentBuilder, SegmentMeta};
use crate::wal::Wal;

/// Result of a completed compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionOutcome {
    pub namespace: String,
    /// Segment ids that were merged away.
    pub inputs: Vec<String>,
    /// Newly written level-1 segment ids.
    pub outputs: Vec<String>,
    pub live_docs: u64,
    pub tombstones_dropped: u64,
    pub manifest_version: u64,
}

/// Result of a GC sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcReport {
    pub namespace: String,
    pub deleted_segments: Vec<String>,
    pub deleted_wal: usize,
    pub deleted_manifests: Vec<u64>,
}

/// Extract the segment id from an object key like `.../segments/<id>.seg`.
fn segment_id_from_key(key: &str) -> Option<String> {
    let name = key.rsplit('/').next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.trim_end_matches(".seg").to_string())
}

/// Rough serialized size of one entry, used to split compaction output at
/// the target segment size.
fn approx_entry_bytes(state: &DocState) -> u64 {
    match state {
        DocState::Tombstone => 48,
        DocState::Live(doc) => {
            serde_json::to_vec(doc).map(|v| v.len() as u64).unwrap_or(256) + 32
        }
    }
}

/// Merge segments into level-1 output(s) and atomically swap the manifest.
/// Returns `None` when the trigger is not met (and `force` is false) or when
/// there is nothing to compact.
pub async fn compact_namespace(
    engine: &Engine,
    ns: &str,
    force: bool,
) -> EngineResult<Option<CompactionOutcome>> {
    let h = engine.handle(ns).await?;
    let _maintenance = h.maintenance.lock().await;

    let base = { h.inner.read().await.manifest.clone() };
    if base.segments.is_empty() {
        return Ok(None);
    }
    let l0_count = base.segments.iter().filter(|s| s.level == 0).count();
    if !force && l0_count < engine.options().l0_compact_trigger {
        return Ok(None);
    }
    // A single already-compacted segment with nothing new is a no-op.
    if !force && base.segments.len() == 1 && l0_count == 0 {
        return Ok(None);
    }

    let inputs: Vec<SegmentMeta> = base.segments.clone();
    let input_ids: Vec<String> = inputs.iter().map(|s| s.id.clone()).collect();

    // 1. Merge: newest LSN wins per document id.
    let mut merged: BTreeMap<DocId, (u64, DocState)> = BTreeMap::new();
    for meta in &inputs {
        let reader = h.cache.get_or_load(engine.store_ref(), ns, meta).await?;
        for entry in reader.iter() {
            let newer = merged.get(&entry.id).map_or(true, |(lsn, _)| entry.lsn > *lsn);
            if newer {
                merged.insert(entry.id.clone(), (entry.lsn, entry.state.clone()));
            }
        }
    }

    // 2. Build outputs, dropping tombstones (full merge => always safe).
    let target = engine.options().compaction_target_segment_bytes.max(1);
    let mut outputs: Vec<(Bytes, SegmentMeta)> = Vec::new();
    let mut current: Option<SegmentBuilder> = None;
    let mut current_bytes: u64 = 0;
    let mut live_docs: u64 = 0;
    let mut tombstones_dropped: u64 = 0;

    for (id, (lsn, state)) in merged {
        if matches!(state, DocState::Tombstone) {
            tombstones_dropped += 1;
            continue;
        }
        if current.is_none() {
            current = Some(SegmentBuilder::new(&segment::new_segment_id(), 1));
            current_bytes = 0;
        }
        current_bytes += approx_entry_bytes(&state);
        current.as_mut().expect("builder present").add(id, lsn, state);
        live_docs += 1;
        if current_bytes >= target {
            outputs.push(current.take().expect("builder present").finish()?);
        }
    }
    if let Some(builder) = current.take() {
        outputs.push(builder.finish()?);
    }
    let output_metas: Vec<SegmentMeta> = outputs.iter().map(|(_, m)| m.clone()).collect();
    let output_ids: Vec<String> = output_metas.iter().map(|m| m.id.clone()).collect();

    // 3. Write outputs.
    failpoint::maybe_fail(&format!("compaction.before_output_put/{ns}"))?;
    for (bytes, meta) in &outputs {
        let created = engine
            .store_ref()
            .put_if_absent(&layout::segment_key(ns, &meta.id), bytes.clone())
            .await?;
        if !created {
            return Err(EngineError::Conflict(format!(
                "compaction output segment id collision for '{}' in namespace '{ns}'",
                meta.id
            )));
        }
    }

    // 4. Atomic commit.
    failpoint::maybe_fail(&format!("compaction.before_manifest_commit/{ns}"))?;
    let mut new_manifest = base.clone();
    new_manifest.version += 1;
    new_manifest.updated_at_ms = now_ms();
    new_manifest.segments = output_metas;
    manifest::commit(engine.store_ref(), &new_manifest).await?;
    failpoint::maybe_fail(&format!("compaction.after_manifest_commit/{ns}"))?;

    // 5. Swap in-memory state, drop cached readers for the dead segments.
    {
        let mut inner = h.inner.write().await;
        inner.manifest = new_manifest.clone();
    }
    h.cache.evict(&input_ids);

    // 6. Delete the inputs (best effort; GC mops up after a crash here).
    failpoint::maybe_fail(&format!("compaction.before_input_delete/{ns}"))?;
    for id in &input_ids {
        engine.store_ref().delete(&layout::segment_key(ns, id)).await?;
    }

    info!(
        namespace = ns,
        inputs = input_ids.len(),
        outputs = output_ids.len(),
        live_docs,
        tombstones_dropped,
        manifest_version = new_manifest.version,
        "compaction complete"
    );

    Ok(Some(CompactionOutcome {
        namespace: ns.to_string(),
        inputs: input_ids,
        outputs: output_ids,
        live_docs,
        tombstones_dropped,
        manifest_version: new_manifest.version,
    }))
}

/// Garbage-collect a namespace:
/// * segment objects unreferenced by the current manifest and older than the
///   grace window (orphans from crashed folds/compactions, or superseded
///   inputs whose deletion was interrupted);
/// * WAL records at or below the manifest watermark (already folded);
/// * manifest versions beyond the configured retention.
pub async fn gc_namespace(engine: &Engine, ns: &str) -> EngineResult<GcReport> {
    let h = engine.handle(ns).await?;
    let _maintenance = h.maintenance.lock().await;
    let current = { h.inner.read().await.manifest.clone() };

    let referenced: HashSet<&str> = current.segments.iter().map(|s| s.id.as_str()).collect();
    let grace = engine.options().gc_grace_ms;
    let now = now_ms();

    // Orphan segments.
    let mut deleted_segments = Vec::new();
    for obj in engine.store_ref().list(&layout::segment_prefix(ns)).await? {
        let Some(id) = segment_id_from_key(&obj.key) else { continue };
        if referenced.contains(id.as_str()) {
            continue;
        }
        if now.saturating_sub(obj.last_modified_ms) < grace {
            debug!(namespace = ns, segment = %id, "orphan within grace window; skipping");
            continue;
        }
        engine.store_ref().delete(&obj.key).await?;
        deleted_segments.push(id);
    }

    // Folded WAL records.
    let wal = Wal::new(engine.store_ref().clone(), ns);
    let deleted_wal = wal.delete_upto(current.last_applied_lsn).await?;

    // Old manifest versions (always keep the current one).
    let mut versions = manifest::list_versions(engine.store_ref(), ns).await?;
    versions.sort_unstable();
    let keep = engine.options().keep_manifests.max(1);
    let mut deleted_manifests = Vec::new();
    if versions.len() > keep {
        let cutoff_index = versions.len() - keep;
        for v in &versions[..cutoff_index] {
            if *v == current.version {
                continue;
            }
            engine.store_ref().delete(&layout::manifest_key(ns, *v)).await?;
            deleted_manifests.push(*v);
        }
    }

    if !deleted_segments.is_empty() || deleted_wal > 0 || !deleted_manifests.is_empty() {
        info!(
            namespace = ns,
            segments = deleted_segments.len(),
            wal = deleted_wal,
            manifests = deleted_manifests.len(),
            "gc sweep complete"
        );
    }

    Ok(GcReport {
        namespace: ns.to_string(),
        deleted_segments,
        deleted_wal,
        deleted_manifests,
    })
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
    use std::collections::BTreeMap as Map;
    use std::sync::Arc;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    fn engine(store: &Arc<dyn ObjectStore>) -> Engine {
        Engine::new(store.clone(), EngineOptions::for_tests())
    }

    fn doc(id: &str, n: i64) -> Document {
        let mut attributes = Map::new();
        attributes.insert("n".to_string(), Value::I64(n));
        Document {
            id: DocId::from(id.to_string()),
            vector: None,
            sparse_vector: None,
            attributes,
        }
    }

    fn id(s: &str) -> DocId {
        DocId::from(s.to_string())
    }

    #[tokio::test]
    async fn compaction_merges_segments_and_drops_tombstones() {
        let ns = "compact_basic";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();

        // Two level-0 segments: one with a..b, one with c plus a tombstone of a.
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();
        e.write(ns, WriteRequest::delete(vec![id("a")])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        assert_eq!(e.stats(ns).await.unwrap().segment_count, 2);

        let outcome = e.compact_now(ns, true).await.unwrap().unwrap();
        assert_eq!(outcome.inputs.len(), 2);
        assert!(!outcome.outputs.is_empty());
        assert_eq!(outcome.live_docs, 2);
        assert_eq!(outcome.tombstones_dropped, 1);

        let stats = e.stats(ns).await.unwrap();
        assert_eq!(stats.level0_segments, 0);
        assert!(e.get_document(ns, &id("a")).await.unwrap().is_none());
        assert_eq!(e.export(ns).await.unwrap().len(), 2);

        // Restart and verify the compacted state is what storage serves.
        let e2 = engine(&store);
        assert!(e2.get_document(ns, &id("a")).await.unwrap().is_none());
        let b = e2.get_document(ns, &id("b")).await.unwrap().unwrap();
        assert_eq!(b.attributes.get("n"), Some(&Value::I64(2)));
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn compaction_triggered_by_l0_count() {
        let ns = "compact_trigger";
        let store = mem();
        let e = engine(&store); // for_tests(): l0_compact_trigger = 2
        e.create_namespace(ns).await.unwrap();

        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        assert!(e.compact_now(ns, false).await.unwrap().is_none(), "below trigger");

        e.write(ns, WriteRequest::upsert(vec![doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        assert!(e.compact_now(ns, false).await.unwrap().is_some(), "trigger met");
        assert!(e.compact_now(ns, false).await.unwrap().is_none(), "idempotent afterwards");
    }

    #[tokio::test]
    async fn crash_before_compaction_manifest_commit_is_safe() {
        let ns = "compact_crash_pre";
        let fp = format!("compaction.before_manifest_commit/{ns}");
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();

        // Crash after outputs are written but before the manifest swap:
        // nothing changed logically; outputs are orphans.
        failpoint::arm(&fp);
        assert!(e.compact_now(ns, true).await.is_err());
        failpoint::disarm(&fp);

        let e2 = engine(&store);
        assert_eq!(e2.stats(ns).await.unwrap().segment_count, 2, "manifest unchanged");
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);

        let gc = e2.gc_now(ns).await.unwrap();
        assert!(!gc.deleted_segments.is_empty(), "orphan outputs collected");

        // Retry succeeds.
        assert!(e2.compact_now(ns, true).await.unwrap().is_some());
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn crash_before_compaction_input_delete_is_safe() {
        let ns = "compact_crash_post";
        let fp = format!("compaction.before_input_delete/{ns}");
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();

        // Crash after the manifest commit but before input deletion: the
        // compaction is durable; the inputs linger as orphans.
        failpoint::arm(&fp);
        assert!(e.compact_now(ns, true).await.is_err());
        failpoint::disarm(&fp);

        let e2 = engine(&store);
        let stats = e2.stats(ns).await.unwrap();
        assert_eq!(stats.level0_segments, 0, "compacted manifest is current");
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);

        let gc = e2.gc_now(ns).await.unwrap();
        assert_eq!(gc.deleted_segments.len(), 2, "orphan inputs collected");
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn writes_during_compaction_window_are_preserved() {
        let ns = "compact_concurrent_writes";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();

        // Unfolded write sitting in the memtable while compaction runs.
        e.write(ns, WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();
        e.compact_now(ns, true).await.unwrap().unwrap();

        let all = e.export(ns).await.unwrap();
        assert_eq!(all.len(), 3, "memtable write must survive compaction");
        // And it folds normally afterwards.
        e.fold_now(ns).await.unwrap().unwrap();
        assert_eq!(engine(&store).export(ns).await.unwrap().len(), 3);
    }
}
