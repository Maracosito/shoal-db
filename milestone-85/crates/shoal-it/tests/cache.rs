//! Cache-hierarchy tests: cold restart correctness, warm-cache endpoint,
//! cold-vs-warm hit behaviour, and pin/unpin.

use serde_json::json;
use shoal_it::{api, basis, ns_name, queries, seed, TestServer, ADMIN_KEY, DIM};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_restart_recovers_from_object_storage() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("cold");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    let warm_vec = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 6), "cosine", 3))
        .await
        .unwrap();
    let warm_txt = srv.top_ids(&ns, queries::text("alpha4", 3)).await.unwrap();
    let warm_ids = srv.export_ids(&ns).await.unwrap();

    // Restart with brand-new, empty caches: everything must be served by
    // re-reading durable state from the object store.
    let srv = srv.restart_cold().await.unwrap();
    assert_eq!(srv.cache_file_count(), 0, "fresh cache dir must start empty");

    let cold_vec = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 6), "cosine", 3))
        .await
        .unwrap();
    let cold_txt = srv.top_ids(&ns, queries::text("alpha4", 3)).await.unwrap();
    let cold_ids = srv.export_ids(&ns).await.unwrap();

    assert_eq!(warm_vec, cold_vec, "vector results must match across cold restart");
    assert_eq!(warm_txt, cold_txt, "text results must match across cold restart");
    assert_eq!(warm_ids, cold_ids, "exported corpus must match across cold restart");
    assert_eq!(cold_vec.first().map(String::as_str), Some("doc-6"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_endpoint_succeeds_and_preserves_results() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("warm");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    // Cold restart so warming actually has something to pull in.
    let srv = srv.restart_cold().await.unwrap();

    let before = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 1), "cosine", 3))
        .await
        .unwrap();

    srv.warm(&ns).await.unwrap();

    let after = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 1), "cosine", 3))
        .await
        .unwrap();
    assert_eq!(before, after, "warming must not change query results");
    assert_eq!(after.first().map(String::as_str), Some("doc-1"));

    // Warming an unknown namespace must fail cleanly.
    let (status, _) = srv
        .post(&api::warm(&ns_name("ghost")), Some(ADMIN_KEY), json!({}))
        .await;
    assert_eq!(status, 404, "warming a missing namespace should 404");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_queries_register_cache_hits() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("hits");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    // Fresh process, empty caches.
    let srv = srv.restart_cold().await.unwrap();

    // First query is the cold one.
    let cold = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 5), "cosine", 3))
        .await
        .unwrap();
    let hits_after_cold = srv.metric_sum(&["cache", "hit"]).await;

    // Warm explicitly, then hammer the identical query.
    srv.warm(&ns).await.unwrap();
    for _ in 0..10 {
        let warm = srv
            .top_ids(&ns, queries::vector(&basis(DIM, 5), "cosine", 3))
            .await
            .unwrap();
        assert_eq!(cold, warm, "warm results must equal cold results");
    }

    let hits_after_warm = srv.metric_sum(&["cache", "hit"]).await;
    assert!(
        hits_after_warm >= hits_after_cold,
        "cache hit counters must be monotonic ({hits_after_cold} -> {hits_after_warm})"
    );
    assert!(
        hits_after_warm > 0.0,
        "expected cache hits after warm + 10 identical queries; metrics:\n{}",
        srv.metrics_text().await
    );

    // The metrics endpoint must actually expose cache instrumentation.
    let text = srv.metrics_text().await;
    assert!(
        text.to_ascii_lowercase().contains("cache"),
        "/metrics should expose cache metrics"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_populates_local_disk_cache() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("populate");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    let srv = srv.restart_cold().await.unwrap();
    assert_eq!(srv.cache_file_count(), 0);

    srv.warm(&ns).await.unwrap();
    // Run a couple of queries as well, in case warm only stages manifests.
    let _ = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 0), "cosine", 3))
        .await
        .unwrap();
    let _ = srv.top_ids(&ns, queries::text("common", 5)).await.unwrap();

    assert!(
        srv.cache_file_count() > 0,
        "warm + queries should have populated the local disk cache at {:?}",
        srv.cache_dir
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_and_unpin_roundtrip() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("pin");
    seed(&srv, &ns, 4).await.unwrap();

    srv.pin(&ns).await.unwrap();

    // If namespace metadata reports pinning, it must say pinned=true here.
    let (status, meta) = srv.get(&api::namespace(&ns), Some(ADMIN_KEY)).await;
    assert!((200..300).contains(&(status as i32)));
    if let Some(p) = meta.get("pinned").and_then(|v| v.as_bool()) {
        assert!(p, "namespace metadata should report pinned=true after pin");
    }

    // Pinned namespaces remain fully usable.
    let ids = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 2), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-2"));

    srv.unpin(&ns).await.unwrap();
    let (status, meta) = srv.get(&api::namespace(&ns), Some(ADMIN_KEY)).await;
    assert!((200..300).contains(&(status as i32)));
    if let Some(p) = meta.get("pinned").and_then(|v| v.as_bool()) {
        assert!(!p, "namespace metadata should report pinned=false after unpin");
    }

    // Pinning a missing namespace must fail.
    let (status, _) = srv
        .post(&api::pin(&ns_name("ghost")), Some(ADMIN_KEY), json!({}))
        .await;
    assert_eq!(status, 404, "pinning a missing namespace should 404");
}
