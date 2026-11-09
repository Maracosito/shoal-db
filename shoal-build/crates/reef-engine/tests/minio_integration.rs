//! End-to-end engine tests against a real S3-compatible store (MinIO).
//!
//! These tests are skipped unless `REEF_TEST_S3_ENDPOINT` is set. Run them
//! locally with `scripts/run-minio-tests.sh`, which starts MinIO via
//! `deploy/docker-compose.test.yml`, creates the test bucket, exports the
//! env vars and invokes this binary. CI runs the same suite (see
//! `.github/workflows/ci.yml`).
//!
//! Every test uses a unique namespace name so suites can share one bucket
//! and repeated runs never collide.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use reef_engine::{Engine, EngineOptions};
use reef_store::{ObjectStore, S3Config, S3Store};
use reef_types::write::WriteOptions;
use reef_types::{Document, Value};

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn unique_ns(tag: &str) -> String {
    format!("it-{tag}-{}", nanos())
}

fn s3_config_from_env() -> Option<S3Config> {
    let endpoint = std::env::var("REEF_TEST_S3_ENDPOINT").ok()?;
    Some(S3Config {
        bucket: std::env::var("REEF_TEST_S3_BUCKET").unwrap_or_else(|_| "reef-test".into()),
        region: std::env::var("REEF_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        endpoint: Some(endpoint),
        access_key_id: Some(
            std::env::var("REEF_TEST_S3_ACCESS_KEY").unwrap_or_else(|_| "reefadmin".into()),
        ),
        secret_access_key: Some(
            std::env::var("REEF_TEST_S3_SECRET_KEY").unwrap_or_else(|_| "reefsecret".into()),
        ),
        force_path_style: true,
    })
}

/// Returns `None` (test skips itself) when no S3 endpoint is configured.
async fn s3_store(test: &str) -> Option<Arc<dyn ObjectStore>> {
    let Some(cfg) = s3_config_from_env() else {
        eprintln!("SKIP {test}: REEF_TEST_S3_ENDPOINT not set");
        return None;
    };
    let store = S3Store::connect(cfg)
        .await
        .expect("connecting to the configured S3 endpoint must succeed");
    Some(Arc::new(store))
}

fn test_options() -> EngineOptions {
    let mut opts = EngineOptions::default();
    opts.gc_grace = Duration::from_secs(0);
    opts
}

async fn open_engine(store: Arc<dyn ObjectStore>) -> Engine {
    Engine::open(store, test_options())
        .await
        .expect("engine should open")
}

fn doc(id: &str, n: i64) -> Document {
    Document::new(id)
        .with_vector(vec![n as f32, 1.0, 0.25])
        .with_attr("n", Value::from(n))
        .with_attr("text", Value::from(format!("minio doc {n}")))
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

// ---------------------------------------------------------------------------

#[tokio::test]
async fn s3_acknowledged_writes_survive_process_restart() {
    let Some(store) = s3_store("s3_acknowledged_writes_survive_process_restart").await else {
        return;
    };
    let ns = unique_ns("restart");

    {
        let engine = open_engine(store.clone()).await;
        engine.create_namespace(&ns).await.unwrap();
        // WAL-only durability: no flush before "crash".
        upsert_range(&engine, &ns, 0, 25).await;
    } // engine dropped — simulates process death

    let engine = open_engine(store.clone()).await;
    assert_eq!(
        scan_ids(&engine, &ns).await,
        expected_ids(0, 25),
        "WAL replay from object storage must restore every acknowledged write"
    );
}

#[tokio::test]
async fn s3_flush_compact_and_cold_reopen() {
    let Some(store) = s3_store("s3_flush_compact_and_cold_reopen").await else {
        return;
    };
    let ns = unique_ns("compact");

    {
        let engine = open_engine(store.clone()).await;
        engine.create_namespace(&ns).await.unwrap();
        for batch in 0..4 {
            upsert_range(&engine, &ns, batch * 25, 25).await;
            engine.flush(&ns).await.unwrap();
        }
        let before = engine.manifest(&ns).await.unwrap();
        assert!(before.segments.len() >= 2);

        engine.compact(&ns).await.unwrap();
        let after = engine.manifest(&ns).await.unwrap();
        assert!(after.segments.len() < before.segments.len());
    }

    let engine = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&engine, &ns).await, expected_ids(0, 100));
    let report = engine.verify(&ns).await.unwrap();
    assert!(report.ok, "verify must be clean on S3: {report:?}");
    assert_eq!(
        engine
            .get(&ns, "doc-00077")
            .await
            .unwrap()
            .expect("doc must exist")
            .attributes
            .get("n"),
        Some(&Value::from(77i64))
    );
}

#[tokio::test]
async fn s3_deletes_and_tombstone_gc() {
    let Some(store) = s3_store("s3_deletes_and_tombstone_gc").await else {
        return;
    };
    let ns = unique_ns("delete");

    let engine = open_engine(store.clone()).await;
    engine.create_namespace(&ns).await.unwrap();
    upsert_range(&engine, &ns, 0, 60).await;
    engine.flush(&ns).await.unwrap();

    let to_delete: Vec<String> = (0..20).map(|i| format!("doc-{i:05}")).collect();
    engine
        .delete(&ns, to_delete.clone(), WriteOptions::default())
        .await
        .unwrap();
    engine.flush(&ns).await.unwrap();
    engine.compact(&ns).await.unwrap();

    drop(engine);
    let engine = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&engine, &ns).await, expected_ids(20, 40));
    for id in &to_delete {
        assert!(engine.get(&ns, id).await.unwrap().is_none());
    }
    let manifest = engine.manifest(&ns).await.unwrap();
    let total_docs: u64 = manifest.segments.iter().map(|s| s.doc_count).sum();
    assert_eq!(total_docs, 40, "tombstone GC must apply on S3 as well");
}

#[tokio::test]
async fn s3_gc_keeps_all_live_data() {
    let Some(store) = s3_store("s3_gc_keeps_all_live_data").await else {
        return;
    };
    let ns = unique_ns("gc");

    {
        let engine = open_engine(store.clone()).await;
        engine.create_namespace(&ns).await.unwrap();
        for batch in 0..5 {
            upsert_range(&engine, &ns, batch * 10, 10).await;
            engine.flush(&ns).await.unwrap();
        }
        engine.compact(&ns).await.unwrap();
        engine.gc(&ns).await.unwrap();
    }

    // Cold reopen from the bucket alone: every live row must be present and
    // verify must be clean — proving GC removed only unreferenced objects.
    let engine = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&engine, &ns).await, expected_ids(0, 50));
    let report = engine.verify(&ns).await.unwrap();
    assert!(report.ok, "post-GC verify must be clean: {report:?}");
}

#[tokio::test]
async fn s3_rebuild_from_object_storage_alone() {
    let Some(store) = s3_store("s3_rebuild_from_object_storage_alone").await else {
        return;
    };
    let ns = unique_ns("rebuild");

    {
        let engine = open_engine(store.clone()).await;
        engine.create_namespace(&ns).await.unwrap();
        upsert_range(&engine, &ns, 0, 40).await;
        engine.flush(&ns).await.unwrap();
        // Leave a second batch in the WAL only.
        upsert_range(&engine, &ns, 40, 10).await;
    }

    // A completely fresh process rebuilds the namespace purely from what is
    // in the bucket: segments + WAL tail.
    let engine = open_engine(store.clone()).await;
    engine.rebuild(&ns).await.expect("rebuild should succeed");
    assert_eq!(scan_ids(&engine, &ns).await, expected_ids(0, 50));
    assert!(engine.verify(&ns).await.unwrap().ok);

    // And once more from cold, to confirm the rebuild was committed durably.
    drop(engine);
    let engine = open_engine(store.clone()).await;
    assert_eq!(scan_ids(&engine, &ns).await, expected_ids(0, 50));
}
