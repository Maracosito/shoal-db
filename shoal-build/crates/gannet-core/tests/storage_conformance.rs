//! Conformance suite for `ObjectStore` backends.
//!
//! The same battery of assertions runs against every backend:
//! - `MemoryStore` and `FilesystemStore` always run.
//! - `S3Store` runs only when `GANNET_TEST_S3_ENDPOINT` is set (use the
//!   provided docker-compose MinIO service):
//!
//! ```sh
//! GANNET_TEST_S3_ENDPOINT=http://localhost:9000 \
//! GANNET_TEST_S3_BUCKET=gannet-test \
//! GANNET_TEST_S3_ACCESS_KEY=minioadmin \
//! GANNET_TEST_S3_SECRET_KEY=minioadmin \
//! cargo test -p gannet-core --test storage_conformance
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use gannet_core::storage::{FilesystemStore, MemoryStore, ObjectStore, S3Config, S3Store};
use gannet_core::StorageError;

async fn conformance(store: &dyn ObjectStore) {
    // --- put / get roundtrip -------------------------------------------------
    store
        .put("docs/one.bin", Bytes::from_static(b"hello world"))
        .await
        .unwrap();
    assert_eq!(
        store.get("docs/one.bin").await.unwrap(),
        Bytes::from_static(b"hello world")
    );

    // --- overwrite -----------------------------------------------------------
    store
        .put("docs/one.bin", Bytes::from_static(b"replaced"))
        .await
        .unwrap();
    assert_eq!(
        store.get("docs/one.bin").await.unwrap(),
        Bytes::from_static(b"replaced")
    );

    // --- get missing ---------------------------------------------------------
    match store.get("docs/missing.bin").await {
        Err(StorageError::NotFound { key }) => assert_eq!(key, "docs/missing.bin"),
        other => panic!("expected NotFound, got {other:?}"),
    }

    // --- head ----------------------------------------------------------------
    let meta = store.head("docs/one.bin").await.unwrap();
    assert_eq!(meta.key, "docs/one.bin");
    assert_eq!(meta.size, "replaced".len() as u64);
    match store.head("docs/missing.bin").await {
        Err(StorageError::NotFound { .. }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    // --- range reads ---------------------------------------------------------
    store
        .put("docs/range.bin", Bytes::from_static(b"0123456789"))
        .await
        .unwrap();
    assert_eq!(
        store.get_range("docs/range.bin", 2..5).await.unwrap(),
        Bytes::from_static(b"234")
    );
    // end past object length clamps
    assert_eq!(
        store.get_range("docs/range.bin", 5..100).await.unwrap(),
        Bytes::from_static(b"56789")
    );
    // degenerate range returns empty
    assert_eq!(
        store.get_range("docs/range.bin", 3..3).await.unwrap(),
        Bytes::new()
    );
    // range on missing object → NotFound
    match store.get_range("docs/missing.bin", 0..4).await {
        Err(StorageError::NotFound { .. }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    // --- put_if_not_exists ---------------------------------------------------
    let created = store
        .put_if_not_exists("manifest/000001.json", Bytes::from_static(b"v1"))
        .await
        .unwrap();
    assert!(created, "first conditional put must create the object");
    let created_again = store
        .put_if_not_exists("manifest/000001.json", Bytes::from_static(b"v2"))
        .await
        .unwrap();
    assert!(!created_again, "second conditional put must report exists");
    assert_eq!(
        store.get("manifest/000001.json").await.unwrap(),
        Bytes::from_static(b"v1"),
        "losing conditional put must not modify the object"
    );

    // --- delete (idempotent) -------------------------------------------------
    store.delete("docs/one.bin").await.unwrap();
    assert!(store.get("docs/one.bin").await.is_err());
    store.delete("docs/one.bin").await.unwrap(); // second delete is fine
    store.delete("never/existed.bin").await.unwrap();

    // --- list ----------------------------------------------------------------
    for key in ["list/a", "list/b/c", "list/b/d", "other/x"] {
        store.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    let all = store.list("list/").await.unwrap();
    let keys: Vec<&str> = all.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys, vec!["list/a", "list/b/c", "list/b/d"]);
    let sub = store.list("list/b/").await.unwrap();
    let keys: Vec<&str> = sub.iter().map(|m| m.key.as_str()).collect();
    assert_eq!(keys, vec!["list/b/c", "list/b/d"]);
    // partial-segment prefix
    let partial = store.list("li").await.unwrap();
    assert_eq!(partial.len(), 3);
    // sizes populated
    assert!(all.iter().all(|m| m.size == 1));
    // empty result for unmatched prefix
    assert!(store.list("zzz/").await.unwrap().is_empty());

    // --- copy ----------------------------------------------------------------
    store
        .put("copy/src.bin", Bytes::from_static(b"payload"))
        .await
        .unwrap();
    store.copy("copy/src.bin", "copy/dst.bin").await.unwrap();
    assert_eq!(
        store.get("copy/dst.bin").await.unwrap(),
        Bytes::from_static(b"payload")
    );
    // source unaffected
    assert_eq!(
        store.get("copy/src.bin").await.unwrap(),
        Bytes::from_static(b"payload")
    );
    // copy of missing source → NotFound
    match store.copy("copy/missing.bin", "copy/dst2.bin").await {
        Err(StorageError::NotFound { .. }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    // --- key validation ------------------------------------------------------
    for bad in ["", "/abs", "a//b", "a/../b", ".tmp/x", "trailing/", "sp ace"] {
        match store.put(bad, Bytes::from_static(b"x")).await {
            Err(StorageError::InvalidKey(_)) => {}
            other => panic!("expected InvalidKey for {bad:?}, got {other:?}"),
        }
    }
    // out-of-bounds range start is an error (object is 10 bytes)
    assert!(store.get_range("docs/range.bin", 10..20).await.is_err());
}

#[tokio::test]
async fn memory_conformance() {
    let store = MemoryStore::new();
    conformance(&store).await;
}

#[tokio::test]
async fn filesystem_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesystemStore::new(dir.path()).unwrap();
    conformance(&store).await;
}

/// Filesystem store contents must survive re-opening the store (the
/// foundation of the crash-recovery story).
#[tokio::test]
async fn filesystem_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = FilesystemStore::new(dir.path()).unwrap();
        store
            .put("ns/docs/wal/00000001.wal", Bytes::from_static(b"entry-1"))
            .await
            .unwrap();
        store
            .put_if_not_exists("ns/docs/manifest/00000001.json", Bytes::from_static(b"m1"))
            .await
            .unwrap();
    }
    // "Restart": open a brand-new store handle over the same root.
    let store = FilesystemStore::new(dir.path()).unwrap();
    assert_eq!(
        store.get("ns/docs/wal/00000001.wal").await.unwrap(),
        Bytes::from_static(b"entry-1")
    );
    let listed = store.list("ns/docs/").await.unwrap();
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn s3_conformance() {
    let Ok(endpoint) = std::env::var("GANNET_TEST_S3_ENDPOINT") else {
        eprintln!("GANNET_TEST_S3_ENDPOINT not set; skipping S3/MinIO conformance test");
        return;
    };
    let bucket =
        std::env::var("GANNET_TEST_S3_BUCKET").unwrap_or_else(|_| "gannet-test".to_string());
    let access_key =
        std::env::var("GANNET_TEST_S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    let secret_key =
        std::env::var("GANNET_TEST_S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".to_string());

    // Unique prefix per run so repeated runs never collide.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let cfg = S3Config {
        bucket,
        prefix: Some(format!("conformance-{nanos}")),
        region: Some("us-east-1".to_string()),
        endpoint: Some(endpoint),
        access_key_id: Some(access_key),
        secret_access_key: Some(secret_key),
        force_path_style: Some(true),
    };
    let store = S3Store::from_config(&cfg).await.unwrap();
    store.ensure_bucket().await.unwrap();
    conformance(&store).await;
}
