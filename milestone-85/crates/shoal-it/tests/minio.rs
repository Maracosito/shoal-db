//! MinIO/S3-backed integration tests exercising the full API surface against
//! real object storage, plus branch refcount safety and cold-restart
//! recovery from S3.
//!
//! These tests are skipped unless `SHOAL_IT_S3_ENDPOINT` is set (see
//! `s3_opts_from_env` in the harness). The configured bucket must already
//! exist; each run isolates itself under a unique key prefix.
//!
//! Local run:
//! ```sh
//! docker compose up -d minio
//! SHOAL_IT_S3_ENDPOINT=http://127.0.0.1:9000 cargo test -p shoal-it --test minio
//! ```

use serde_json::json;
use shoal_it::{
    api, assert_ids, basis, doc, filters, ns_name, queries, s3_opts_from_env, seed, TestServer,
    ADMIN_KEY, DIM, READER_KEY,
};

macro_rules! s3_or_skip {
    () => {
        match s3_opts_from_env() {
            Some(opts) => opts,
            None => {
                eprintln!("skipping MinIO test: SHOAL_IT_S3_ENDPOINT not set");
                return;
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minio_full_api_surface() {
    let opts = s3_or_skip!();
    let srv = TestServer::spawn_s3(opts).await.unwrap();

    // System endpoints.
    let (status, _) = srv.get_text(api::HEALTH, None).await;
    assert_eq!(status, 200);
    let (status, text) = srv.get_text(api::METRICS, None).await;
    assert_eq!(status, 200);
    assert!(text.contains("# TYPE") || text.contains("# HELP"));

    // Namespace CRUD against S3.
    let ns = ns_name("s3");
    seed(&srv, &ns, 8).await.unwrap();
    let (status, body) = srv.get(&api::namespaces(), Some(READER_KEY)).await;
    assert!((200..300).contains(&(status as i32)));
    assert!(serde_json::to_string(&body).unwrap().contains(&ns));

    // Force segment materialisation in object storage where supported.
    srv.try_compact(&ns).await;

    // Queries: vector / text / hybrid / filtered.
    let ids = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 6), "cosine", 3))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-6"));

    let ids = srv.top_ids(&ns, queries::text("alpha2", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-2"));

    let q = queries::hybrid(&basis(DIM, 1), "cosine", "alpha7", queries::rrf(), 5);
    let ids = srv.top_ids(&ns, q).await.unwrap();
    let top2: Vec<&str> = ids.iter().take(2).map(String::as_str).collect();
    assert!(top2.contains(&"doc-1") && top2.contains(&"doc-7"), "got {ids:?}");

    let q = queries::with_filter(
        queries::text("common", 20),
        filters::and(vec![
            filters::eq("lang", json!("en")),
            filters::gte("rank", json!(4)),
        ]),
    );
    let mut ids = srv.top_ids(&ns, q).await.unwrap();
    ids.sort();
    assert_eq!(ids, vec!["doc-4".to_string(), "doc-6".to_string()]);

    // Patch + delete-by-id + delete-by-filter.
    srv.patch_docs(&ns, &[json!({"id": "doc-0", "attributes": {"lang": "de"}})])
        .await
        .unwrap();
    srv.delete_by_ids(&ns, &["doc-7"]).await.unwrap();
    srv.delete_by_filter(&ns, filters::eq("lang", json!("de")))
        .await
        .unwrap();
    assert_ids(&srv, &ns, &["doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "doc-6"])
        .await
        .unwrap();

    // Export, copy, warm, pin.
    let copy = ns_name("s3-copy");
    srv.copy(&ns, &copy).await.unwrap();
    assert_ids(&srv, &copy, &["doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "doc-6"])
        .await
        .unwrap();
    srv.warm(&ns).await.unwrap();
    srv.pin(&ns).await.unwrap();
    srv.unpin(&ns).await.unwrap();

    // Cold restart against the same S3 prefix with empty caches: all durable
    // state must be recoverable purely from object storage.
    let srv = srv.restart_cold().await.unwrap();
    assert_ids(&srv, &ns, &["doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "doc-6"])
        .await
        .unwrap();
    let ids = srv
        .top_ids(&ns, queries::vector(&basis(DIM, 4), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-4"));
    let ids = srv.top_ids(&ns, queries::text("alpha5", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-5"));

    // Cleanup.
    srv.delete_namespace(&copy).await.unwrap();
    srv.delete_namespace(&ns).await.unwrap();
    let (status, _) = srv.get(&api::namespace(&ns), Some(ADMIN_KEY)).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn minio_branch_isolation_and_refcount_safety() {
    let opts = s3_or_skip!();
    let srv = TestServer::spawn_s3(opts).await.unwrap();

    let a = ns_name("s3-src");
    let b = ns_name("s3-br");
    seed(&srv, &a, 6).await.unwrap();
    srv.try_compact(&a).await;
    srv.branch(&a, &b).await.unwrap();

    // Bidirectional isolation over S3.
    srv.upsert(&a, &[doc("only-a", basis(DIM, 7), "OnlyA", "only-in-a quebec", json!({}))])
        .await
        .unwrap();
    srv.upsert(&b, &[doc("only-b", basis(DIM, 7), "OnlyB", "only-in-b romeo", json!({}))])
        .await
        .unwrap();
    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "only-a"])
        .await
        .unwrap();
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "only-b"])
        .await
        .unwrap();

    // Delete the source. Shared segments in S3 must survive via refcounts.
    srv.delete_namespace(&a).await.unwrap();
    let ids = srv
        .top_ids(&b, queries::vector(&basis(DIM, 3), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-3"));
    let ids = srv.top_ids(&b, queries::text("alpha0", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-0"));

    // And the branch must survive a cold restart from S3 after orphaning.
    let srv = srv.restart_cold().await.unwrap();
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-5", "only-b"])
        .await
        .unwrap();
    let ids = srv
        .top_ids(&b, queries::vector(&basis(DIM, 5), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-5"));

    srv.delete_namespace(&b).await.unwrap();
}
