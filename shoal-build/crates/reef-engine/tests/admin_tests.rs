//! Admin command tests: `verify`, `repair`, and `rebuild`.
//!
//! Covers healthy namespaces (verify is clean, rebuild is loss-free) and
//! damaged namespaces (corrupted segment payloads, missing segment objects,
//! orphaned objects left behind by crashes).

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use reef_engine::{layout, Engine, EngineOptions};
use reef_store::{MemoryStore, ObjectStore};
use reef_types::write::WriteOptions;
use reef_types::{Document, Value};

const NS: &str = "admin-ns";

fn doc(id: &str, n: i64) -> Document {
    Document::new(id)
        .with_vector(vec![n as f32, 0.5])
        .with_attr("n", Value::from(n))
}

async fn open_engine(store: Arc<dyn ObjectStore>) -> Engine {
    Engine::open(store, EngineOptions::default())
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

async fn first_segment_key(store: &Arc<MemoryStore>, ns: &str) -> String {
    let objects = store
        .list(&layout::segment_prefix(ns))
        .await
        .expect("list should succeed");
    assert!(!objects.is_empty(), "expected at least one segment object");
    objects[0].key.clone()
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_is_clean_on_healthy_namespace() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 60).await;
    engine.flush(NS).await.unwrap();
    upsert_range(&engine, NS, 60, 40).await;
    engine.flush(NS).await.unwrap();
    engine.compact(NS).await.unwrap();

    let report = engine.verify(NS).await.expect("verify should run");
    assert!(report.ok, "healthy namespace must verify clean: {report:?}");
    assert!(report.issues.is_empty(), "no issues expected: {report:?}");
}

#[tokio::test]
async fn verify_is_clean_on_empty_namespace() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    let report = engine.verify(NS).await.expect("verify should run");
    assert!(report.ok, "empty namespace must verify clean: {report:?}");
}

#[tokio::test]
async fn verify_detects_corrupted_segment_payload() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 50).await;
    engine.flush(NS).await.unwrap();

    // Overwrite a live segment object with garbage; the checksum / framing
    // validation in verify must catch it.
    let victim = first_segment_key(&store, NS).await;
    store
        .put(&victim, Bytes::from_static(b"this is not a reef segment"))
        .await
        .unwrap();

    let report = engine.verify(NS).await.expect("verify should run");
    assert!(!report.ok, "corruption must be detected: {report:?}");
    assert!(!report.issues.is_empty());
}

#[tokio::test]
async fn verify_detects_missing_segment_object() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 50).await;
    engine.flush(NS).await.unwrap();

    let victim = first_segment_key(&store, NS).await;
    store.delete(&victim).await.unwrap();

    let report = engine.verify(NS).await.expect("verify should run");
    assert!(!report.ok, "missing segment must be detected: {report:?}");
}

#[tokio::test]
async fn verify_flags_orphan_objects() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 20).await;
    engine.flush(NS).await.unwrap();

    // Simulate a crash that left a partially-written segment object behind
    // that no manifest references.
    let prefix = layout::segment_prefix(NS);
    let orphan = format!("{}/orphan-deadbeef.seg", prefix.trim_end_matches('/'));
    store
        .put(&orphan, Bytes::from_static(b"\x00\x01incomplete"))
        .await
        .unwrap();

    let report = engine.verify(NS).await.expect("verify should run");
    assert!(
        !report.issues.is_empty(),
        "orphan object must be reported as an issue: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// repair
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repair_removes_orphan_objects() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 20).await;
    engine.flush(NS).await.unwrap();

    let prefix = layout::segment_prefix(NS);
    let orphan = format!("{}/orphan-cafebabe.seg", prefix.trim_end_matches('/'));
    store
        .put(&orphan, Bytes::from_static(b"junk left by a crash"))
        .await
        .unwrap();

    let _report = engine.repair(NS).await.expect("repair should run");

    // The orphan must be gone from its original location...
    assert!(
        store.get(&orphan).await.is_err(),
        "repair must remove (or quarantine away) the orphan object"
    );
    // ...and verify must come back clean with all live data intact.
    let report = engine.verify(NS).await.unwrap();
    assert!(report.ok, "post-repair verify must be clean: {report:?}");
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 20));
}

#[tokio::test]
async fn repair_quarantines_unreadable_segment_and_keeps_namespace_usable() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    // Two independently flushed batches -> at least two segments, so damage
    // to one segment cannot take down the whole namespace.
    upsert_range(&engine, NS, 0, 50).await;
    engine.flush(NS).await.unwrap();
    upsert_range(&engine, NS, 50, 50).await;
    engine.flush(NS).await.unwrap();

    let victim = first_segment_key(&store, NS).await;
    store
        .put(&victim, Bytes::from_static(b"corrupted beyond recovery"))
        .await
        .unwrap();

    let _report = engine.repair(NS).await.expect("repair should run");

    // After repair the namespace must verify clean again...
    let report = engine.verify(NS).await.unwrap();
    assert!(report.ok, "post-repair verify must be clean: {report:?}");

    // ...reads must not error, and surviving rows must be a subset of what
    // was written (the unrecoverable segment's rows may be lost — repair is
    // about restoring invariants, not inventing data).
    let surviving = scan_ids(&engine, NS).await;
    let all = expected_ids(0, 100);
    assert!(
        surviving.is_subset(&all),
        "repair must never fabricate rows"
    );

    // The namespace must remain fully writable afterwards.
    upsert_range(&engine, NS, 100, 10).await;
    engine.flush(NS).await.unwrap();
    let after_write = scan_ids(&engine, NS).await;
    for id in expected_ids(100, 10) {
        assert!(after_write.contains(&id), "new write {id} must be visible");
    }
}

#[tokio::test]
async fn repair_on_healthy_namespace_changes_nothing() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 30).await;
    engine.flush(NS).await.unwrap();

    let before = scan_ids(&engine, NS).await;
    let _report = engine.repair(NS).await.expect("repair should run");
    let after = scan_ids(&engine, NS).await;

    assert_eq!(before, after, "repair on a healthy namespace is a no-op");
    assert!(engine.verify(NS).await.unwrap().ok);
}

// ---------------------------------------------------------------------------
// rebuild
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rebuild_is_loss_free_on_healthy_namespace() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    upsert_range(&engine, NS, 0, 80).await;
    engine.flush(NS).await.unwrap();
    engine.compact(NS).await.unwrap();

    let v_before = engine.manifest(NS).await.unwrap().version;
    let _report = engine.rebuild(NS).await.expect("rebuild should run");
    let v_after = engine.manifest(NS).await.unwrap().version;

    assert!(
        v_after >= v_before,
        "rebuild must never roll the manifest backwards"
    );
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 80));
    assert_eq!(
        engine
            .get(NS, "doc-00042")
            .await
            .unwrap()
            .expect("doc must exist")
            .attributes
            .get("n"),
        Some(&Value::from(42i64))
    );

    // A cold reopen from object storage alone must agree with the rebuilt
    // state — proving the rebuild was committed durably.
    drop(engine);
    let reopened = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 80));
    assert!(reopened.verify(NS).await.unwrap().ok);
}

#[tokio::test]
async fn rebuild_folds_unflushed_wal() {
    let store = Arc::new(MemoryStore::new());
    let engine = open_engine(store.clone()).await;
    engine.create_namespace(NS).await.unwrap();

    // WAL-only state: durable but not yet folded into segments.
    upsert_range(&engine, NS, 0, 35).await;

    let _report = engine.rebuild(NS).await.expect("rebuild should run");
    assert_eq!(scan_ids(&engine, NS).await, expected_ids(0, 35));

    drop(engine);
    let reopened = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&reopened, NS).await, expected_ids(0, 35));
    assert!(reopened.verify(NS).await.unwrap().ok);
}
