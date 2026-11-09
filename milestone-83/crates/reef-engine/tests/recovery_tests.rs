//! Crash-recovery and durability tests.
//!
//! Covers the full milestone matrix:
//!   * restart after unflushed writes (pure WAL replay)
//!   * restart after flush (segments + manifest)
//!   * crash mid-indexing (segment upload fails; manifest commit fails)
//!   * crash mid-compaction (segment write fails; manifest commit fails)
//!   * corrupted WAL objects, corrupted segments, orphaned files
//!   * full reconstruction of a namespace from object storage alone
//!
//! Failure injection uses `FailpointStore`, which wraps a real store and
//! fails the Nth `put` whose key matches a substring (one-shot, so the
//! "process restart" after the simulated crash sees a healthy store again).

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use reef_engine::{admin, Engine, EngineOptions};
use reef_store::{FailpointStore, MemoryStore, ObjectStore};
use reef_types::DocId;

/// A memory-backed store wrapped in a failpoint layer.
fn failpoint_store() -> (Arc<FailpointStore>, Arc<dyn ObjectStore>) {
    let base: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let fp = Arc::new(FailpointStore::new(base));
    let store: Arc<dyn ObjectStore> = fp.clone();
    (fp, store)
}

// ---------------------------------------------------------------------------
// Clean restarts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_after_unflushed_writes_replays_wal() {
    for ts in all_backends() {
        let ns = "rec-unflushed";
        {
            let engine = open_engine(&ts.store, ns).await;
            engine
                .write(upsert_batch(numbered_docs("d", 25)))
                .await
                .unwrap();
            // Drop without flushing: durability must come from the WAL alone.
        }
        let engine = open_engine(&ts.store, ns).await;
        assert_eq!(doc_count(&engine).await, 25, "backend={}", ts.name);
        let d = engine
            .get(&DocId::from("d00003"))
            .await
            .unwrap()
            .expect("d00003 after restart");
        assert_eq!(d.attributes.get("n"), Some(&i(3)));
        assert_unique_ids(&engine).await;
    }
}

#[tokio::test]
async fn restart_after_flush_serves_from_segments() {
    for ts in all_backends() {
        let ns = "rec-flushed";
        {
            let engine = open_engine(&ts.store, ns).await;
            engine
                .write(upsert_batch(numbered_docs("d", 40)))
                .await
                .unwrap();
            engine.flush().await.unwrap();
        }
        let engine = open_engine(&ts.store, ns).await;
        assert_eq!(doc_count(&engine).await, 40, "backend={}", ts.name);
        assert!(
            !engine.manifest_snapshot().await.unwrap().segments.is_empty(),
            "recovered manifest must reference the flushed segment, backend={}",
            ts.name
        );

        // The recovered engine must remain fully writable.
        engine
            .write(upsert_batch(vec![doc("post-restart")]))
            .await
            .unwrap();
        assert_eq!(doc_count(&engine).await, 41);
    }
}

#[tokio::test]
async fn restart_with_mixed_flushed_and_unflushed_state() {
    for ts in all_backends() {
        let ns = "rec-mixed";
        {
            let engine = open_engine(&ts.store, ns).await;
            engine
                .write(upsert_batch(numbered_docs("flushed", 10)))
                .await
                .unwrap();
            engine.flush().await.unwrap();
            engine
                .write(upsert_batch(numbered_docs("pending", 7)))
                .await
                .unwrap();
            engine.write(delete_batch(&["flushed00009"])).await.unwrap();
            // Crash: segment + manifest hold the first batch; WAL holds the rest.
        }
        let engine = open_engine(&ts.store, ns).await;
        assert_eq!(
            doc_count(&engine).await,
            16, // 10 flushed - 1 deleted + 7 pending
            "backend={}",
            ts.name
        );
        assert!(engine
            .get(&DocId::from("flushed00009"))
            .await
            .unwrap()
            .is_none());
        assert!(engine
            .get(&DocId::from("pending00006"))
            .await
            .unwrap()
            .is_some());
        assert_unique_ids(&engine).await;
    }
}

// ---------------------------------------------------------------------------
// Crash mid-indexing (flush)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_during_indexing_segment_upload_is_recoverable() {
    let (fp, store) = failpoint_store();
    let ns = "rec-idx-seg";

    let engine = open_engine(&store, ns).await;
    engine
        .write(upsert_batch(numbered_docs("d", 30)))
        .await
        .unwrap();

    // The next segment object upload fails — simulating a crash while the
    // indexer is writing the segment file.
    fp.fail_nth_put_matching("segments/", 1);
    assert!(
        engine.flush().await.is_err(),
        "flush must surface the injected segment-upload failure"
    );
    drop(engine);

    // "Restart": the failpoint was one-shot, so the store is healthy again.
    let engine = open_engine(&store, ns).await;
    assert_eq!(doc_count(&engine).await, 30, "WAL must still cover all writes");

    // Retrying the indexing pass must succeed and must not duplicate anything.
    engine.flush().await.unwrap();
    assert_eq!(doc_count(&engine).await, 30);
    assert_unique_ids(&engine).await;
    assert!(!engine.manifest_snapshot().await.unwrap().segments.is_empty());
}

#[tokio::test]
async fn crash_during_indexing_manifest_commit_is_recoverable() {
    let (fp, store) = failpoint_store();
    let ns = "rec-idx-manifest";

    let engine = open_engine(&store, ns).await;
    engine
        .write(upsert_batch(numbered_docs("d", 30)))
        .await
        .unwrap();

    // Segment upload succeeds, but the manifest commit fails — the worst
    // partial state: an orphan segment exists that no manifest references.
    fp.fail_nth_put_matching("manifest", 1);
    assert!(
        engine.flush().await.is_err(),
        "flush must surface the injected manifest-commit failure"
    );
    drop(engine);

    let engine = open_engine(&store, ns).await;
    // The uncommitted segment must be invisible; the WAL is still authoritative.
    assert_eq!(doc_count(&engine).await, 30);
    assert_unique_ids(&engine).await;

    // Retried flush commits cleanly.
    engine.flush().await.unwrap();
    assert_eq!(doc_count(&engine).await, 30);
    assert_unique_ids(&engine).await;
    drop(engine);

    // The orphan from the failed attempt is repairable, and repairing it
    // must not lose any data.
    admin::repair(store.clone(), ns).await.unwrap();
    let report = admin::verify(store.clone(), ns).await.unwrap();
    assert!(
        report.healthy,
        "namespace must verify clean after repair: {report:?}"
    );
    let engine = open_engine(&store, ns).await;
    assert_eq!(doc_count(&engine).await, 30);
}

#[tokio::test]
async fn wal_upload_failure_leaves_namespace_unchanged() {
    let (fp, store) = failpoint_store();
    let ns = "rec-wal-put";

    let engine = open_engine(&store, ns).await;
    engine
        .write(upsert_batch(vec![doc("committed")]))
        .await
        .unwrap();

    // The next WAL object upload fails: the write must be rejected and must
    // not become visible, because durability comes before acknowledgement.
    fp.fail_nth_put_matching("wal", 1);
    assert!(engine
        .write(upsert_batch(vec![doc("never-durable")]))
        .await
        .is_err());
    assert_eq!(doc_count(&engine).await, 1);
    assert!(engine
        .get(&DocId::from("never-durable"))
        .await
        .unwrap()
        .is_none());
    drop(engine);

    // After restart the rejected write must still be absent and the engine
    // must accept new writes.
    let engine = open_engine(&store, ns).await;
    assert_eq!(doc_count(&engine).await, 1);
    engine.write(upsert_batch(vec![doc("retry")])).await.unwrap();
    assert_eq!(doc_count(&engine).await, 2);
}

// ---------------------------------------------------------------------------
// Crash mid-compaction
// ---------------------------------------------------------------------------

/// Build three small segments in the namespace and return the engine.
async fn engine_with_three_segments(store: &Arc<dyn ObjectStore>, ns: &str) -> Engine {
    let engine = open_engine(store, ns).await;
    for chunk in 0..3usize {
        engine
            .write(upsert_batch(numbered_docs(&format!("c{chunk}-"), 10)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
    }
    let manifest = engine.manifest_snapshot().await.unwrap();
    assert!(
        manifest.segments.len() >= 3,
        "setup should yield at least 3 segments, got {}",
        manifest.segments.len()
    );
    engine
}

#[tokio::test]
async fn crash_during_compaction_manifest_commit_keeps_old_manifest() {
    let (fp, store) = failpoint_store();
    let ns = "rec-compact-manifest";
    let engine = engine_with_three_segments(&store, ns).await;
    let before = engine.manifest_snapshot().await.unwrap();

    // Compaction writes the merged segment but fails to commit the manifest.
    fp.fail_nth_put_matching("manifest", 1);
    assert!(
        engine.compact().await.is_err(),
        "compaction must surface the injected manifest-commit failure"
    );

    // The previous manifest must be fully intact: same version, same segments.
    let after_fail = engine.manifest_snapshot().await.unwrap();
    assert_eq!(after_fail.version, before.version);
    assert_eq!(after_fail.segments.len(), before.segments.len());
    assert_eq!(doc_count(&engine).await, 30);
    drop(engine);

    // After restart the namespace is healthy and compaction can be retried.
    let engine = open_engine(&store, ns).await;
    assert_eq!(doc_count(&engine).await, 30);
    engine.compact().await.unwrap();
    let after = engine.manifest_snapshot().await.unwrap();
    assert!(
        after.segments.len() < before.segments.len(),
        "retried compaction should reduce segment count ({} -> {})",
        before.segments.len(),
        after.segments.len()
    );
    assert_eq!(doc_count(&engine).await, 30);
    assert_unique_ids(&engine).await;
    let d = engine.get(&DocId::from("c1-00004")).await.unwrap().unwrap();
    assert_eq!(d.attributes.get("n"), Some(&i(4)));
}

#[tokio::test]
async fn crash_during_compaction_segment_write_keeps_inputs_intact() {
    let (fp, store) = failpoint_store();
    let ns = "rec-compact-seg";
    let engine = engine_with_three_segments(&store, ns).await;
    let before = engine.manifest_snapshot().await.unwrap();

    // The merged-segment upload itself fails.
    fp.fail_nth_put_matching("segments/", 1);
    assert!(engine.compact().await.is_err());

    // Inputs untouched, data fully readable.
    let after_fail = engine.manifest_snapshot().await.unwrap();
    assert_eq!(after_fail.version, before.version);
    assert_eq!(after_fail.segments.len(), before.segments.len());
    assert_eq!(doc_count(&engine).await, 30);
    drop(engine);

    let engine = open_engine(&store, ns).await;
    engine.compact().await.unwrap();
    assert_eq!(doc_count(&engine).await, 30);
    assert_unique_ids(&engine).await;
}

// ---------------------------------------------------------------------------
// Corruption and orphan detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_wal_object_is_detected_and_repairable() {
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let ns = "rec-corrupt-wal";
    {
        let engine = open_engine(&store, ns).await;
        // Two separate batches => two separate immutable WAL objects.
        engine
            .write(upsert_batch(numbered_docs("a", 10)))
            .await
            .unwrap();
        engine
            .write(upsert_batch(numbered_docs("b", 10)))
            .await
            .unwrap();
        // Crash without flushing.
    }

    let mut wal_keys = list_keys_containing(&store, "wal").await;
    wal_keys.retain(|k| k.contains(ns));
    assert!(
        wal_keys.len() >= 2,
        "expected at least two WAL objects, got {wal_keys:?}"
    );
    // WAL objects are sequence-named, so the lexicographically last key is
    // the newest batch (the `b*` docs). Corrupt it in place.
    let victim = wal_keys.last().unwrap().clone();
    store
        .put(&victim, Bytes::from_static(b"this is not a valid wal record"))
        .await
        .unwrap();

    // Strict recovery: opening must refuse to silently skip a corrupt record.
    assert!(
        Engine::open(store.clone(), ns, EngineOptions::default())
            .await
            .is_err(),
        "open must surface WAL corruption"
    );

    // Repair quarantines the corrupt object; everything before it survives.
    admin::repair(store.clone(), ns).await.unwrap();
    let engine = open_engine(&store, ns).await;
    let ids = scan_ids(&engine).await;
    assert_eq!(ids.len(), 10, "the intact first batch must survive: {ids:?}");
    assert!(
        ids.iter().all(|id| id.starts_with('a')),
        "only pre-corruption documents should remain: {ids:?}"
    );
    // And the repaired namespace must accept new writes.
    engine.write(upsert_batch(vec![doc("after-repair")])).await.unwrap();
    assert_eq!(doc_count(&engine).await, 11);
}

#[tokio::test]
async fn corrupt_segment_is_detected_by_verify() {
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let ns = "rec-corrupt-seg";
    {
        let engine = open_engine(&store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("d", 20)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
    }

    let mut seg_keys = list_keys_containing(&store, "segments/").await;
    seg_keys.retain(|k| k.contains(ns));
    assert!(!seg_keys.is_empty(), "flush should have produced a segment");
    overwrite_with_garbage(&store, &seg_keys[0]).await;

    let report = admin::verify(store.clone(), ns).await.unwrap();
    assert!(
        !report.healthy,
        "verify must flag the corrupted segment: {report:?}"
    );
    assert!(
        !report.issues.is_empty(),
        "verify must report at least one concrete issue"
    );
}

#[tokio::test]
async fn orphaned_segment_file_is_detected_and_repaired() {
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let ns = "rec-orphan";
    {
        let engine = open_engine(&store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("d", 10)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
    }

    // Plant an orphan next to the real segment: an object in the segments
    // directory that no manifest references (e.g. a leftover from a crashed
    // indexing pass).
    let mut seg_keys = list_keys_containing(&store, "segments/").await;
    seg_keys.retain(|k| k.contains(ns));
    let real = seg_keys[0].clone();
    let dir = &real[..real.rfind('/').unwrap()];
    let orphan_key = format!("{dir}/zzzz-orphan-leftover");
    store
        .put(&orphan_key, Bytes::from_static(b"partial leftover bytes"))
        .await
        .unwrap();

    let report = admin::verify(store.clone(), ns).await.unwrap();
    assert!(
        report.orphan_files.iter().any(|k| k.contains("zzzz-orphan")),
        "verify must list the orphan: {report:?}"
    );

    admin::repair(store.clone(), ns).await.unwrap();
    let remaining = list_keys_containing(&store, "zzzz-orphan").await;
    assert!(remaining.is_empty(), "repair must remove the orphan");

    // Live data untouched.
    let engine = open_engine(&store, ns).await;
    assert_eq!(doc_count(&engine).await, 10);
    let report = admin::verify(store.clone(), ns).await.unwrap();
    assert!(report.healthy, "namespace must verify clean after repair");
}

#[tokio::test]
async fn manifest_can_be_rebuilt_from_segments_and_wal() {
    let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let ns = "rec-rebuild";
    let expected;
    {
        let engine = open_engine(&store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("seg", 15)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
        engine
            .write(upsert_batch(numbered_docs("wal", 5)))
            .await
            .unwrap();
        engine.write(delete_batch(&["seg00000"])).await.unwrap();
        expected = snapshot(&engine).await;
    }

    // Destroy every manifest object — simulating manifest loss/corruption.
    let mut manifest_keys = list_keys_containing(&store, "manifest").await;
    manifest_keys.retain(|k| k.contains(ns));
    assert!(!manifest_keys.is_empty(), "expected manifest objects to exist");
    for key in &manifest_keys {
        store.delete(key).await.unwrap();
    }

    // Rebuild from the immutable segment and WAL objects alone.
    admin::rebuild_manifest(store.clone(), ns).await.unwrap();
    let engine = open_engine(&store, ns).await;
    assert_eq!(
        snapshot(&engine).await,
        expected,
        "rebuilt namespace must be byte-for-byte equivalent"
    );
    assert_unique_ids(&engine).await;
}

// ---------------------------------------------------------------------------
// Full reconstruction from object storage alone
// ---------------------------------------------------------------------------

#[tokio::test]
async fn namespace_is_fully_reconstructable_from_object_storage_alone() {
    // Drive a realistic lifecycle on a localfs store: multiple flush cycles,
    // a patch, deletes, and a compaction.
    let ts = localfs_backend();
    let ns = "rec-clone";
    let expected;
    {
        let engine = open_engine(&ts.store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("d", 30)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
        engine
            .write(upsert_batch(numbered_docs("e", 30)))
            .await
            .unwrap();
        engine.flush().await.unwrap();
        engine
            .write(patch_batch(vec![reef_types::Patch {
                id: DocId::from("d00001"),
                set: std::collections::BTreeMap::from([("patched".to_string(), b(true))]),
                unset: vec![],
                vector: None,
            }]))
            .await
            .unwrap();
        engine
            .write(delete_batch(&["d00002", "e00029"]))
            .await
            .unwrap();
        engine.flush().await.unwrap();
        engine.compact().await.unwrap();
        // Leave one batch unflushed so the clone exercises WAL replay too.
        engine
            .write(upsert_batch(numbered_docs("tail", 4)))
            .await
            .unwrap();
        expected = snapshot(&engine).await;
    }

    // Copy raw objects into a completely fresh store — nothing from the old
    // process (memory, local caches, file handles) carries over.
    let clone = clone_into_memory(&ts.store).await;
    let engine = open_engine(&clone, ns).await;

    assert_eq!(
        snapshot(&engine).await,
        expected,
        "clone reconstructed from raw objects must match the source exactly"
    );
    assert_eq!(doc_count(&engine).await, 62); // 60 + 4 - 2 deleted
    let patched = engine.get(&DocId::from("d00001")).await.unwrap().unwrap();
    assert_eq!(patched.attributes.get("patched"), Some(&b(true)));
    assert!(engine.get(&DocId::from("d00002")).await.unwrap().is_none());

    // The reconstructed namespace must be fully operational.
    engine.write(upsert_batch(vec![doc("post-clone")])).await.unwrap();
    engine.flush().await.unwrap();
    engine.compact().await.unwrap();
    assert_eq!(doc_count(&engine).await, 63);
    assert_unique_ids(&engine).await;
}
