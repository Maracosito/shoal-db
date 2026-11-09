//! Shared test harness for the reef-engine integration test suite.
//!
//! Every test in this suite goes through the *public* engine API only, so the
//! suite doubles as a contract test for the surface that the HTTP server and
//! CLI build on. Tests run against an in-memory object store and a local
//! filesystem object store (a tempdir per backend); the MinIO/S3 suite lives
//! in `minio_integration.rs` and is gated by environment variables.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use reef_engine::{Engine, EngineOptions};
use reef_store::{LocalFsStore, MemoryStore, ObjectStore};
use reef_types::{DocId, Document, Patch, Value, WriteBatch};

/// A named object-store backend so tests can run the same scenario across a
/// backend matrix and report which backend failed.
pub struct TestStore {
    pub name: &'static str,
    pub store: Arc<dyn ObjectStore>,
    // Keep the tempdir alive for the duration of the test.
    _tmp: Option<tempfile::TempDir>,
}

pub fn memory_backend() -> TestStore {
    TestStore {
        name: "memory",
        store: Arc::new(MemoryStore::new()),
        _tmp: None,
    }
}

pub fn localfs_backend() -> TestStore {
    let tmp = tempfile::tempdir().expect("create tempdir for localfs backend");
    let store: Arc<dyn ObjectStore> = Arc::new(LocalFsStore::new(tmp.path()));
    TestStore {
        name: "localfs",
        store,
        _tmp: Some(tmp),
    }
}

/// All backends every test should pass against by default.
pub fn all_backends() -> Vec<TestStore> {
    vec![memory_backend(), localfs_backend()]
}

// ---------------------------------------------------------------------------
// Engine helpers
// ---------------------------------------------------------------------------

/// Open an engine over a namespace with default options. Opening performs
/// recovery (manifest load + WAL replay), so calling this after dropping a
/// previous engine simulates a clean process restart.
pub async fn open_engine(store: &Arc<dyn ObjectStore>, namespace: &str) -> Engine {
    Engine::open(store.clone(), namespace, EngineOptions::default())
        .await
        .expect("engine open should succeed")
}

pub async fn doc_count(engine: &Engine) -> usize {
    engine.doc_count().await.expect("doc_count")
}

/// All live document ids in the namespace, sorted.
pub async fn scan_ids(engine: &Engine) -> Vec<String> {
    let mut ids: Vec<String> = engine
        .scan_all()
        .await
        .expect("scan_all")
        .into_iter()
        .map(|d| d.id.to_string())
        .collect();
    ids.sort();
    ids
}

/// Assert that recovery / compaction never produced duplicate live ids.
pub async fn assert_unique_ids(engine: &Engine) {
    let ids = scan_ids(engine).await;
    let mut dedup = ids.clone();
    dedup.dedup();
    assert_eq!(
        ids.len(),
        dedup.len(),
        "duplicate document ids in scan: {ids:?}"
    );
}

/// A comparable, deterministic snapshot of every live document.
pub async fn snapshot(
    engine: &Engine,
) -> Vec<(String, Option<Vec<f32>>, BTreeMap<String, Value>)> {
    let mut rows: Vec<_> = engine
        .scan_all()
        .await
        .expect("scan_all")
        .into_iter()
        .map(|d| (d.id.to_string(), d.vector.clone(), d.attributes.clone()))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

// ---------------------------------------------------------------------------
// Value / document / batch builders
// ---------------------------------------------------------------------------

pub fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

pub fn i(v: i64) -> Value {
    Value::I64(v)
}

pub fn f(v: f64) -> Value {
    Value::F64(v)
}

pub fn b(v: bool) -> Value {
    Value::Bool(v)
}

pub fn doc(id: &str) -> Document {
    Document {
        id: DocId::from(id),
        vector: None,
        sparse_vector: None,
        attributes: BTreeMap::new(),
    }
}

pub fn doc_attrs(id: &str, attrs: &[(&str, Value)]) -> Document {
    let mut d = doc(id);
    for (k, v) in attrs {
        d.attributes.insert((*k).to_string(), v.clone());
    }
    d
}

pub fn doc_vec(id: &str, vector: Vec<f32>, attrs: &[(&str, Value)]) -> Document {
    let mut d = doc_attrs(id, attrs);
    d.vector = Some(vector);
    d
}

/// `n` documents with ids `{prefix}00000..{prefix}{n-1:05}`, an integer
/// attribute `n` and a string attribute `group` in `g0..g4`.
pub fn numbered_docs(prefix: &str, n: usize) -> Vec<Document> {
    (0..n)
        .map(|idx| {
            doc_attrs(
                &format!("{prefix}{idx:05}"),
                &[
                    ("n", i(idx as i64)),
                    ("group", s(&format!("g{}", idx % 5))),
                ],
            )
        })
        .collect()
}

pub fn upsert_batch(docs: Vec<Document>) -> WriteBatch {
    WriteBatch {
        upserts: docs,
        ..WriteBatch::default()
    }
}

pub fn delete_batch(ids: &[&str]) -> WriteBatch {
    WriteBatch {
        deletes: ids.iter().map(|id| DocId::from(*id)).collect(),
        ..WriteBatch::default()
    }
}

pub fn patch_batch(patches: Vec<Patch>) -> WriteBatch {
    WriteBatch {
        patches,
        ..WriteBatch::default()
    }
}

// ---------------------------------------------------------------------------
// Raw object-store helpers (corruption injection, cloning, inspection)
// ---------------------------------------------------------------------------

/// All keys in the store whose path contains `needle`, sorted. The engine's
/// layout puts WAL objects under a `wal/` directory, segments under
/// `segments/`, and manifests under `manifest/`, so tests use those needles
/// without depending on the exact key format.
pub async fn list_keys_containing(store: &Arc<dyn ObjectStore>, needle: &str) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list("")
        .await
        .expect("list objects")
        .into_iter()
        .map(|m| m.key)
        .filter(|k| k.contains(needle))
        .collect();
    keys.sort();
    keys
}

/// Overwrite an object in place with same-length garbage (every byte XORed),
/// guaranteeing a checksum mismatch without changing object size.
pub async fn overwrite_with_garbage(store: &Arc<dyn ObjectStore>, key: &str) {
    let original = store.get(key).await.expect("read object before corrupting");
    let garbage: Vec<u8> = original.iter().map(|byte| byte ^ 0xA5).collect();
    store
        .put(key, Bytes::from(garbage))
        .await
        .expect("write corrupted object");
}

/// Copy every object into a fresh in-memory store. Used to prove that a
/// namespace is fully reconstructable from object storage alone: nothing the
/// old process held in memory or on local disk is carried over.
pub async fn clone_into_memory(store: &Arc<dyn ObjectStore>) -> Arc<dyn ObjectStore> {
    let dst: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    for meta in store.list("").await.expect("list source store") {
        let data = store.get(&meta.key).await.expect("read source object");
        dst.put(&meta.key, data).await.expect("write cloned object");
    }
    dst
}
