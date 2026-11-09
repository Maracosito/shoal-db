//! Black-box conformance suite for every `ObjectStore` implementation.
//!
//! The same battery runs against:
//!   * `MemoryStore`  — always
//!   * `LocalFsStore` — always (in a throwaway temp directory)
//!   * `S3Store`      — only when `REEF_TEST_S3_ENDPOINT` is set
//!                       (see `scripts/run-minio-tests.sh`)
//!
//! The engine's correctness arguments (atomic manifest swap, WAL durability,
//! GC safety) rest entirely on the semantics asserted here.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use reef_store::{LocalFsStore, MemoryStore, ObjectStore, S3Config, S3Store};

/// Heuristic "not found" check that works regardless of the concrete error
/// enum shape: every store maps missing keys to an error whose Display
/// mentions the key being absent.
fn looks_not_found(err: &dyn std::fmt::Display) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("not found") || s.contains("notfound") || s.contains("no such")
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

// ---------------------------------------------------------------------------
// The shared battery
// ---------------------------------------------------------------------------

async fn run_conformance(store: Arc<dyn ObjectStore>, prefix: &str) {
    let k = |s: &str| format!("{prefix}/{s}");

    // --- put / get roundtrip --------------------------------------------
    store
        .put(&k("a/one"), Bytes::from_static(b"hello reef"))
        .await
        .expect("put must succeed");
    let got = store.get(&k("a/one")).await.expect("get must succeed");
    assert_eq!(&got[..], b"hello reef");

    // --- get of a missing key is a NotFound-style error -------------------
    let err = store
        .get(&k("does/not/exist"))
        .await
        .expect_err("get of missing key must fail");
    assert!(
        looks_not_found(&err),
        "missing key must surface as not-found, got: {err}"
    );

    // --- overwrite replaces content fully ---------------------------------
    store
        .put(&k("a/one"), Bytes::from_static(b"v2"))
        .await
        .unwrap();
    let got = store.get(&k("a/one")).await.unwrap();
    assert_eq!(&got[..], b"v2", "overwrite must fully replace content");

    // --- binary content with embedded zero bytes --------------------------
    let binary = Bytes::from(vec![0u8, 255, 0, 1, 2, 0, 254]);
    store.put(&k("a/binary"), binary.clone()).await.unwrap();
    assert_eq!(store.get(&k("a/binary")).await.unwrap(), binary);

    // --- a 1 MiB object survives intact ------------------------------------
    let big: Vec<u8> = (0..1_048_576usize).map(|i| (i % 251) as u8).collect();
    store
        .put(&k("a/big"), Bytes::from(big.clone()))
        .await
        .unwrap();
    let got = store.get(&k("a/big")).await.unwrap();
    assert_eq!(got.len(), big.len());
    assert_eq!(&got[..], &big[..]);

    // --- list returns exactly the keys under a prefix ----------------------
    store
        .put(&k("b/nested/x"), Bytes::from_static(b"x"))
        .await
        .unwrap();
    store
        .put(&k("b/nested/y"), Bytes::from_static(b"y"))
        .await
        .unwrap();
    store.put(&k("b/z"), Bytes::from_static(b"z")).await.unwrap();
    store
        .put(&k("c/outside"), Bytes::from_static(b"o"))
        .await
        .unwrap();

    let listed: BTreeSet<String> = store
        .list(&k("b/"))
        .await
        .expect("list must succeed")
        .into_iter()
        .map(|o| o.key)
        .collect();
    let expected: BTreeSet<String> =
        [k("b/nested/x"), k("b/nested/y"), k("b/z")].into_iter().collect();
    assert_eq!(
        listed, expected,
        "list must return exactly the keys under the prefix (recursively)"
    );

    // --- list of an empty prefix is empty, not an error --------------------
    let empty = store.list(&k("nothing-here/")).await.unwrap();
    assert!(empty.is_empty());

    // --- delete removes the object; deleting a missing key is idempotent ---
    store.delete(&k("b/z")).await.expect("delete must succeed");
    let err = store.get(&k("b/z")).await.expect_err("deleted key is gone");
    assert!(looks_not_found(&err));
    store
        .delete(&k("b/z"))
        .await
        .expect("deleting an already-missing key must be a no-op");

    let listed: BTreeSet<String> = store
        .list(&k("b/"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect();
    assert!(!listed.contains(&k("b/z")));

    // --- put_if_absent: first wins, second fails, content untouched --------
    store
        .put_if_absent(&k("locks/leader"), Bytes::from_static(b"first"))
        .await
        .expect("first put_if_absent must succeed");
    let second = store
        .put_if_absent(&k("locks/leader"), Bytes::from_static(b"second"))
        .await;
    assert!(
        second.is_err(),
        "second put_if_absent on an existing key must fail"
    );
    let got = store.get(&k("locks/leader")).await.unwrap();
    assert_eq!(
        &got[..], b"first",
        "losing put_if_absent must not modify the object"
    );

    // After deletion the key is claimable again.
    store.delete(&k("locks/leader")).await.unwrap();
    store
        .put_if_absent(&k("locks/leader"), Bytes::from_static(b"third"))
        .await
        .expect("put_if_absent must succeed again after delete");
    assert_eq!(&store.get(&k("locks/leader")).await.unwrap()[..], b"third");
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_store_conformance() {
    run_conformance(Arc::new(MemoryStore::new()), "conf").await;
}

// ---------------------------------------------------------------------------
// LocalFsStore
// ---------------------------------------------------------------------------

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "reef-store-test-{tag}-{}-{}",
        std::process::id(),
        nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn localfs_store_conformance() {
    let root = temp_root("conf");
    {
        let store = LocalFsStore::new(&root).expect("open localfs store");
        run_conformance(Arc::new(store), "conf").await;
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn localfs_store_persists_across_reopen() {
    let root = temp_root("reopen");
    {
        let store = LocalFsStore::new(&root).expect("open localfs store");
        store
            .put("ns/data/seg-001", Bytes::from_static(b"durable bytes"))
            .await
            .unwrap();
        store
            .put("ns/manifest/000001", Bytes::from_static(b"{\"v\":1}"))
            .await
            .unwrap();
    } // store dropped — simulates process exit

    {
        let store = LocalFsStore::new(&root).expect("reopen localfs store");
        let got = store.get("ns/data/seg-001").await.unwrap();
        assert_eq!(&got[..], b"durable bytes");
        let keys: BTreeSet<String> = store
            .list("ns/")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(
            keys,
            ["ns/data/seg-001".to_string(), "ns/manifest/000001".to_string()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// S3Store (gated: only runs when REEF_TEST_S3_ENDPOINT is set)
// ---------------------------------------------------------------------------

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

#[tokio::test]
async fn s3_store_conformance() {
    let Some(cfg) = s3_config_from_env() else {
        eprintln!("SKIP s3_store_conformance: REEF_TEST_S3_ENDPOINT not set");
        return;
    };
    let store = S3Store::connect(cfg).await.expect("connect to MinIO/S3");
    // Unique prefix so repeated runs against a shared bucket never collide.
    let prefix = format!("conf-{}", nanos());
    run_conformance(Arc::new(store), &prefix).await;
}
