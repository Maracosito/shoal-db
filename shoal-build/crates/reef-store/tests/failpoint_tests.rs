//! Unit tests for the `FailpointStore` fault-injection wrapper.
//!
//! These tests pin down the exact crash semantics the engine's recovery
//! tests rely on: `ErrorBefore` (nothing written), `ErrorAfter` (written but
//! ack lost), and `Truncate(n)` (torn write — only a prefix of the bytes
//! landed, and the caller saw an error).

use std::sync::Arc;

use bytes::Bytes;
use reef_store::{FailpointStore, FailureMode, MemoryStore, ObjectStore};

fn setup() -> (Arc<MemoryStore>, FailpointStore) {
    let inner = Arc::new(MemoryStore::new());
    let fp = FailpointStore::new(inner.clone());
    (inner, fp)
}

#[tokio::test]
async fn unarmed_store_passes_through() {
    let (inner, fp) = setup();

    fp.put("a/b", Bytes::from_static(b"data")).await.unwrap();
    assert_eq!(&fp.get("a/b").await.unwrap()[..], b"data");
    assert_eq!(&inner.get("a/b").await.unwrap()[..], b"data");

    let keys = fp.list("a/").await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].key, "a/b");

    fp.delete("a/b").await.unwrap();
    assert!(inner.get("a/b").await.is_err());
}

#[tokio::test]
async fn error_before_put_writes_nothing() {
    let (inner, fp) = setup();

    fp.fail_next_put("wal/", FailureMode::ErrorBefore);
    let err = fp
        .put("ns/wal/000001", Bytes::from_static(b"entry"))
        .await
        .expect_err("armed put must fail");
    let _ = err; // error surfaced to the caller

    assert!(
        inner.get("ns/wal/000001").await.is_err(),
        "ErrorBefore must leave no object behind"
    );
}

#[tokio::test]
async fn error_after_put_persists_object_but_reports_failure() {
    let (inner, fp) = setup();

    // Simulates a lost ack: the PUT durably landed, but the writer crashed
    // (or the response was dropped) before observing success.
    fp.fail_next_put("manifest", FailureMode::ErrorAfter);
    fp.put("ns/manifest/000002", Bytes::from_static(b"{\"v\":2}"))
        .await
        .expect_err("armed put must report failure");

    let got = inner.get("ns/manifest/000002").await.unwrap();
    assert_eq!(&got[..], b"{\"v\":2}", "ErrorAfter must persist the object");
}

#[tokio::test]
async fn truncate_produces_a_torn_write() {
    let (inner, fp) = setup();

    fp.fail_next_put("seg", FailureMode::Truncate(4));
    fp.put("ns/segments/seg-001", Bytes::from_static(b"0123456789"))
        .await
        .expect_err("torn write must report failure to the writer");

    let got = inner.get("ns/segments/seg-001").await.unwrap();
    assert_eq!(
        &got[..], b"0123",
        "Truncate(4) must persist exactly the first 4 bytes"
    );
}

#[tokio::test]
async fn failpoints_are_one_shot() {
    let (inner, fp) = setup();

    fp.fail_next_put("k", FailureMode::ErrorBefore);
    assert!(fp.put("k1", Bytes::from_static(b"a")).await.is_err());

    // The failpoint was consumed: the retry must succeed.
    fp.put("k1", Bytes::from_static(b"a")).await.unwrap();
    assert_eq!(&inner.get("k1").await.unwrap()[..], b"a");
}

#[tokio::test]
async fn failpoints_match_only_their_key_substring() {
    let (inner, fp) = setup();

    fp.fail_next_put("manifest", FailureMode::ErrorBefore);

    // A put to an unrelated key must sail through and must NOT consume
    // the armed failpoint.
    fp.put("ns/segments/seg-001", Bytes::from_static(b"seg"))
        .await
        .unwrap();
    assert_eq!(&inner.get("ns/segments/seg-001").await.unwrap()[..], b"seg");

    // The matching key still trips.
    assert!(fp
        .put("ns/manifest/000003", Bytes::from_static(b"m"))
        .await
        .is_err());
    assert!(inner.get("ns/manifest/000003").await.is_err());
}

#[tokio::test]
async fn get_failpoint_fails_reads_once() {
    let (_inner, fp) = setup();

    fp.put("ns/data", Bytes::from_static(b"payload")).await.unwrap();

    fp.fail_next_get("ns/data", FailureMode::ErrorBefore);
    assert!(fp.get("ns/data").await.is_err(), "armed get must fail");
    assert_eq!(
        &fp.get("ns/data").await.unwrap()[..],
        b"payload",
        "subsequent get must succeed"
    );
}

#[tokio::test]
async fn delete_failpoint_with_lost_ack_still_deletes() {
    let (inner, fp) = setup();

    fp.put("ns/old-seg", Bytes::from_static(b"x")).await.unwrap();

    // GC issues a DELETE, the object is removed, but the ack is lost. The
    // caller must treat the delete as failed and retry safely.
    fp.fail_next_delete("old-seg", FailureMode::ErrorAfter);
    assert!(fp.delete("ns/old-seg").await.is_err());
    assert!(
        inner.get("ns/old-seg").await.is_err(),
        "ErrorAfter on delete must still remove the object"
    );

    // The retry (now unarmed) is an idempotent no-op.
    fp.delete("ns/old-seg").await.unwrap();
}

#[tokio::test]
async fn clear_failpoints_disarms_everything() {
    let (inner, fp) = setup();

    fp.fail_next_put("a", FailureMode::ErrorBefore);
    fp.fail_next_get("a", FailureMode::ErrorBefore);
    fp.fail_next_delete("a", FailureMode::ErrorBefore);
    fp.clear_failpoints();

    fp.put("a/key", Bytes::from_static(b"v")).await.unwrap();
    assert_eq!(&fp.get("a/key").await.unwrap()[..], b"v");
    fp.delete("a/key").await.unwrap();
    assert!(inner.get("a/key").await.is_err());
}
