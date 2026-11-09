//! Compaction-behavior test suite.
//!
//! Proves, against the in-memory object store (the same code paths run on
//! local FS and S3):
//!   * flush folds WAL entries into immutable segments
//!   * the merge policy collapses many small segments into few large ones
//!   * tombstones and shadowed row versions are physically dropped by a
//!     full compaction (tombstone GC)
//!   * manifest updates are atomic: a failed manifest write never leaves a
//!     half-applied compaction visible, and the manifest version is strictly
//!     monotonic across flush/compact/reopen
//!   * obsolete segment objects are garbage-collected after compaction,
//!     while every object still referenced by the live manifest survives

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use reef_engine::{layout, Engine, EngineOptions};
use reef_store::{FailpointStore, FailureMode, MemoryStore, ObjectStore};
use reef_types::write::WriteOptions;
use reef_types::{Document, Value};

const NS: &str = "compaction-ns";

fn test_options() -> EngineOptions {
    let mut opts = EngineOptions::default();
    // Tests assert on GC effects immediately after `gc()`, so disable the
    // grace window that protects in-flight readers in production.
    opts.gc_grace = Duration::from_secs(0);
    opts
}

fn doc(id: &str, n: i64) -> Document {
    Document::new(id)
        .with_vector(vec![n as f32, 1.0, -2.5])
        .with_attr("n", Value::from(n))
        .with_attr("text", Value::from(format!("document number {n}")))
}

async fn open_engine(store: Arc<dyn ObjectStore>) -> Engine {
    Engine::open(store, test_options())
        .await
        .expect("engine should open")
}

async fn upsert_range(engine: &Engine, ns: &str, start: i64, count: i64) {
    let docs: Vec<Document> = (start..start + count)
        .map(|i| doc(&format!("doc-{i:05}"), i))
        .collect();
    engine
        .upsert(ns, docs, WriteOptions::default())
        .await
        .expect("upsert should succeed");
}

async fn scan_ids(engine: &Engine, ns: &str) -> BTreeSet<String> {
    engine
        .scan(ns)
        .await
        .expect("scan should succeed")
        .into_iter()
        .map(|d| d.id.to_string())
        .collect()
}

fn expected_ids(start: i64, count: i64) -> BTreeSet<String> {
    (start..start + count)
        .map(|i| format!("doc-{i:05}"))
        .collect()
}

async fn segment_object_count(store: &Arc<MemoryStore>, ns: &str) -> usize {
    store
        .list(&layout::segment_prefix(ns))
        .await
        .expect("list should succeed")
        .len()
}

// ---------------------------------------------------------------------------
// Flush / merge policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn flush_folds_wal_into_segments() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 100).await;
    engine.flush(NS).await.expect("flush should succeed");

    let manifest = engine.manifest(NS).await.expect("manifest readable");
    assert!(
        !manifest.segments.is_empty(),
        "flush must register at least one segment in the manifest"
    );
    let total_docs: u64 = manifest.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(total_docs, 100, "segment doc counts must cover every row");

    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 100));
}

#[tokio::test]
async fn compaction_merges_small_segments() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    // Produce 8 small segments via repeated upsert + flush.
    for batch in 0..8 {
        upsert_range(&engine, NS, batch * 10, 10).await;
        engine.flush(NS).await.unwrap();
    }

    let before = engine.manifest(NS).await.unwrap();
    assert!(
        before.segments.len() >= 2,
        "test setup must produce multiple segments, got {}",
        before.segments.len()
    );

    let _report = engine.compact(NS).await.expect("compaction should succeed");

    let after = engine.manifest(NS).await.unwrap();
    assert!(
        after.segments.len() < before.segments.len(),
        "compaction must reduce segment count ({} -> {})",
        before.segments.len(),
        after.segments.len()
    );
    let total_docs: u64 = after.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(total_docs, 80, "compaction must not lose or duplicate rows");
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 80));
}

#[tokio::test]
async fn repeated_compaction_is_a_safe_noop() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 50).await;
    engine.flush(NS).await.unwrap();
    engine.compact(NS).await.unwrap();
    let ids_once = scan_ids(&engine, NS).await;

    // Compacting an already-compacted namespace must not change visible data.
    engine.compact(NS).await.unwrap();
    let ids_twice = scan_ids(&engine, NS).await;

    assert_eq!(ids_once, ids_twice);
    assert_eq!(ids_twice, expected_ids(0, 50));
}

// ---------------------------------------------------------------------------
// Tombstone GC and version resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compaction_drops_tombstones_and_deleted_rows() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 100).await;
    engine.flush(NS).await.unwrap();

    // Delete 40 documents; the deletes land as tombstones in their own segment.
    let to_delete: Vec<String> = (0..40).map(|i| format!("doc-{i:05}")).collect();
    engine
        .delete(NS, to_delete.clone(), WriteOptions::default())
        .await
        .expect("delete should succeed");
    engine.flush(NS).await.unwrap();

    // Deletes must be visible before compaction.
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(40, 60));

    engine.compact(NS).await.unwrap();

    // After a full compaction the deleted rows and their tombstones are
    // physically gone: live segments account for exactly the surviving rows.
    let manifest = engine.manifest(NS).await.unwrap();
    let total_docs: u64 = manifest.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(
        total_docs, 60,
        "tombstones/shadowed rows must be dropped by full compaction"
    );
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(40, 60));
    for id in &to_delete {
        assert!(
            engine.get(NS, id).await.unwrap().is_none(),
            "deleted doc {id} must stay deleted after compaction"
        );
    }
}

#[tokio::test]
async fn compaction_keeps_only_latest_row_version() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    // Two generations of the same document across two segments.
    let v1 = Document::new("doc-a").with_attr("v", Value::from(1i64));
    engine
        .upsert(NS, vec![v1], WriteOptions::default())
        .await
        .unwrap();
    engine.flush(NS).await.unwrap();

    let v2 = Document::new("doc-a").with_attr("v", Value::from(2i64));
    engine
        .upsert(NS, vec![v2], WriteOptions::default())
        .await
        .unwrap();
    engine.flush(NS).await.unwrap();

    engine.compact(NS).await.unwrap();

    let got = engine
        .get(NS, "doc-a")
        .await
        .unwrap()
        .expect("doc-a must survive compaction");
    assert_eq!(got.attributes.get("v"), Some(&Value::from(2i64)));

    let manifest = engine.manifest(NS).await.unwrap();
    let total_docs: u64 = manifest.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(total_docs, 1, "shadowed version must be physically dropped");
}

// ---------------------------------------------------------------------------
// Manifest atomicity and version monotonicity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_version_is_strictly_monotonic() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    let mut versions: Vec<u64> = Vec::new();
    versions.push(engine.manifest(NS).await.unwrap().version);

    for batch in 0..4 {
        upsert_range(&engine, NS, batch * 10, 10).await;
        engine.flush(NS).await.unwrap();
        versions.push(engine.manifest(NS).await.unwrap().version);
    }

    engine.compact(NS).await.unwrap();
    versions.push(engine.manifest(NS).await.unwrap().version);

    for pair in versions.windows(2) {
        assert!(
            pair[1] > pair[0],
            "manifest version must strictly increase on every committed \
             mutation: {versions:?}"
        );
    }

    // Reopening from object storage must never observe an older manifest.
    drop(engine);
    let reopened = open_engine(store.clone()).await;
    let v_reopen = reopened.manifest(NS).await.unwrap().version;
    assert!(
        v_reopen >= *versions.last().unwrap(),
        "reopen must see the latest committed manifest ({} < {})",
        v_reopen,
        versions.last().unwrap()
    );
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 40));
}

#[tokio::test]
async fn failed_manifest_write_aborts_compaction_atomically() {
    let inner = Arc::new(MemoryStore::new());
    let fp = Arc::new(FailpointStore::new(inner.clone()));
    let engine = open_engine(fp.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    for batch in 0..4 {
        upsert_range(&engine, NS, batch * 10, 10).await;
        engine.flush(NS).await.unwrap();
    }
    let before = engine.manifest(NS).await.unwrap();

    // Fail the next manifest object write outright: the new merged segment
    // may have been uploaded, but the commit point (manifest swap) must not
    // be reached, so nothing changes for readers.
    fp.fail_next_put("manifest", FailureMode::ErrorBefore);
    let result = engine.compact(NS).await;
    assert!(result.is_err(), "compaction must surface the storage error");

    let after_failure = engine.manifest(NS).await.unwrap();
    assert_eq!(
        after_failure.version, before.version,
        "a failed compaction must not advance the manifest"
    );
    assert_eq!(
        after_failure.segments.len(),
        before.segments.len(),
        "a failed compaction must not change the visible segment set"
    );
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 40));

    // A cold reopen straight from the underlying store must agree.
    let reopened = open_engine(inner.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 40));
    drop(reopened);

    // With the failpoint consumed, retrying the compaction succeeds.
    let _report = engine.compact(NS).await.expect("retry must succeed");
    let after_retry = engine.manifest(NS).await.unwrap();
    assert!(after_retry.version > before.version);
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 40));
}

#[tokio::test]
async fn manifest_write_with_lost_ack_is_safe_after_restart() {
    let inner = Arc::new(MemoryStore::new());
    let fp = Arc::new(FailpointStore::new(inner.clone()));
    let engine = open_engine(fp.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 30).await;

    // The manifest object IS written but the writer sees an error
    // (simulates a lost ack / crash immediately after the PUT).
    fp.fail_next_put("manifest", FailureMode::ErrorAfter);
    let _ = engine.flush(NS).await; // may report an error; that is fine
    drop(engine);

    // Whichever side of the commit point the namespace landed on, a restart
    // must converge to exactly the acknowledged data: 30 docs, no loss, no
    // duplication (WAL replay + manifest reconcile).
    let reopened = open_engine(inner.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 30));

    // And the namespace must remain fully writable afterwards.
    upsert_range(&reopened, NS, 30, 10).await;
    reopened.flush(NS).await.unwrap();
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 40));
}

// ---------------------------------------------------------------------------
// Obsolete-file garbage collection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gc_removes_obsolete_segments_after_compaction() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    for batch in 0..6 {
        upsert_range(&engine, NS, batch * 10, 10).await;
        engine.flush(NS).await.unwrap();
    }
    let objects_before_compact = segment_object_count(&store, NS).await;
    assert!(objects_before_compact > 0);

    engine.compact(NS).await.unwrap();

    // Compaction itself must NOT delete the superseded segments; they remain
    // until GC so that readers holding the old manifest stay correct.
    let objects_after_compact = segment_object_count(&store, NS).await;
    assert!(
        objects_after_compact >= objects_before_compact,
        "compaction must defer deletion to GC \
         (before={objects_before_compact}, after={objects_after_compact})"
    );

    engine.gc(NS).await.expect("gc should succeed");

    let objects_after_gc = segment_object_count(&store, NS).await;
    assert!(
        objects_after_gc < objects_after_compact,
        "gc must remove segments no longer referenced by the manifest \
         (after_compact={objects_after_compact}, after_gc={objects_after_gc})"
    );

    // Every object the live manifest references must have survived: a cold
    // reopen from object storage alone still serves every row.
    drop(engine);
    let reopened = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 60));
    let manifest = reopened.manifest(NS).await.unwrap();
    let total_docs: u64 = manifest.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(total_docs, 60);
}

#[tokio::test]
async fn gc_on_quiescent_namespace_deletes_nothing_live() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 25).await;
    engine.flush(NS).await.unwrap();
    engine.compact(NS).await.unwrap();
    engine.gc(NS).await.unwrap();

    // Run GC again with nothing obsolete: object count must be stable and
    // data fully intact.
    let count_before = segment_object_count(&store, NS).await;
    engine.gc(NS).await.unwrap();
    let count_after = segment_object_count(&store, NS).await;
    assert_eq!(count_before, count_after, "idempotent gc must be a no-op");

    drop(engine);
    let reopened = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 25));
}
