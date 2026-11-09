//! Full API-surface tests against the local-filesystem backend: health,
//! metrics, auth/roles, namespace CRUD, document lifecycle, every query
//! mode, copy-namespace independence, and error handling.

use serde_json::json;
use shoal_it::{
    api, assert_ids, basis, corpus, doc, doc_attr, extract_hits, filters, ns_name, queries, seed,
    TestServer, ADMIN_KEY, DIM, READER_KEY, WRITER_KEY,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_metrics_endpoints() {
    let srv = TestServer::spawn_local().await.unwrap();

    let (status, _) = srv.get_text(api::HEALTH, None).await;
    assert_eq!(status, 200, "healthz must be unauthenticated and healthy");

    let (status, text) = srv.get_text(api::METRICS, None).await;
    assert_eq!(status, 200, "metrics endpoint must respond");
    assert!(
        text.contains("# TYPE") || text.contains("# HELP"),
        "metrics output should be Prometheus exposition format"
    );
    // Secret-safe: API keys must never appear in metrics output.
    for key in [ADMIN_KEY, WRITER_KEY, READER_KEY] {
        assert!(!text.contains(key), "API key leaked into /metrics");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_and_roles_are_enforced() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("auth");
    seed(&srv, &ns, 4).await.unwrap();

    // Missing key.
    let (status, _) = srv.get(&api::namespaces(), None).await;
    assert_eq!(status, 401, "missing API key must be 401");

    // Unknown key.
    let (status, _) = srv.get(&api::namespaces(), Some("not-a-real-key")).await;
    assert_eq!(status, 401, "unknown API key must be 401");

    // Reader cannot create namespaces.
    let (status, _) = srv
        .post(&api::namespaces(), Some(READER_KEY), json!({"name": ns_name("nope")}))
        .await;
    assert_eq!(status, 403, "reader must not create namespaces");

    // Reader cannot write documents.
    let (status, _) = srv
        .post(
            &api::documents(&ns),
            Some(READER_KEY),
            json!({"documents": corpus(1)}),
        )
        .await;
    assert_eq!(status, 403, "reader must not upsert documents");

    // Reader cannot delete documents.
    let (status, _) = srv
        .post(&api::documents_delete(&ns), Some(READER_KEY), json!({"ids": ["doc-0"]}))
        .await;
    assert_eq!(status, 403, "reader must not delete documents");

    // Reader CAN query and export.
    let (status, _) = srv
        .post(&api::query(&ns), Some(READER_KEY), queries::text("common", 3))
        .await;
    assert!((200..300).contains(&(status as i32)), "reader must be able to query");
    let (status, _) = srv.get_text(&api::export(&ns), Some(READER_KEY)).await;
    assert!((200..300).contains(&(status as i32)), "reader must be able to export");

    // Writer CAN upsert but must not delete namespaces.
    let (status, _) = srv
        .post(
            &api::documents(&ns),
            Some(WRITER_KEY),
            json!({"documents": [doc("doc-w", basis(DIM, 7), "W", "writer doc", json!({}))]}),
        )
        .await;
    assert!((200..300).contains(&(status as i32)), "writer must be able to upsert");
    let (status, _) = srv.delete(&api::namespace(&ns), Some(WRITER_KEY)).await;
    assert_eq!(status, 403, "writer must not delete namespaces");

    // Admin can delete namespaces.
    let (status, _) = srv.delete(&api::namespace(&ns), Some(ADMIN_KEY)).await;
    assert!((200..300).contains(&(status as i32)), "admin must be able to delete namespaces");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespace_crud_and_listing() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("crud");

    srv.create_namespace(&ns).await.unwrap();

    // Duplicate creation must be rejected.
    let (status, _) = srv
        .post(&api::namespaces(), Some(ADMIN_KEY), json!({"name": &ns}))
        .await;
    assert!(
        status == 409 || status == 400,
        "duplicate namespace creation should be 409/400, got {status}"
    );

    // List contains the namespace.
    let (status, body) = srv.get(&api::namespaces(), Some(READER_KEY)).await;
    assert!((200..300).contains(&(status as i32)));
    let listed = serde_json::to_string(&body).unwrap();
    assert!(listed.contains(&ns), "namespace list should contain {ns}");

    // Metadata fetch works; missing namespace 404s.
    let (status, _) = srv.get(&api::namespace(&ns), Some(READER_KEY)).await;
    assert!((200..300).contains(&(status as i32)));
    let (status, _) = srv.get(&api::namespace(&ns_name("ghost")), Some(READER_KEY)).await;
    assert_eq!(status, 404);

    // Delete, then verify gone.
    srv.delete_namespace(&ns).await.unwrap();
    let (status, _) = srv.get(&api::namespace(&ns), Some(READER_KEY)).await;
    assert_eq!(status, 404, "deleted namespace must 404");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_lifecycle_upsert_patch_delete() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("lifecycle");
    seed(&srv, &ns, 6).await.unwrap();
    assert_ids(&srv, &ns, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-5"])
        .await
        .unwrap();

    // Patch: change one attribute, keep the rest.
    srv.patch_docs(&ns, &[json!({"id": "doc-1", "attributes": {"lang": "de"}})])
        .await
        .unwrap();
    let docs = srv.export_docs(&ns).await.unwrap();
    let d1 = docs
        .iter()
        .find(|d| shoal_it::hit_id(d).as_deref() == Some("doc-1"))
        .unwrap();
    assert_eq!(doc_attr(d1, "lang").and_then(|v| v.as_str()), Some("de"));
    assert_eq!(
        doc_attr(d1, "title").and_then(|v| v.as_str()),
        Some("Title 1"),
        "patch must not clobber unpatched attributes"
    );

    // Delete by ID.
    srv.delete_by_ids(&ns, &["doc-0"]).await.unwrap();
    assert_ids(&srv, &ns, &["doc-1", "doc-2", "doc-3", "doc-4", "doc-5"])
        .await
        .unwrap();

    // Delete by filter (removes the patched doc-1, lang=de).
    srv.delete_by_filter(&ns, filters::eq("lang", json!("de")))
        .await
        .unwrap();
    assert_ids(&srv, &ns, &["doc-2", "doc-3", "doc-4", "doc-5"])
        .await
        .unwrap();

    // Re-upserting an existing document is idempotent on the ID set.
    srv.upsert(&ns, &[corpus(6)[2].clone()]).await.unwrap();
    assert_ids(&srv, &ns, &["doc-2", "doc-3", "doc-4", "doc-5"])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_modes_vector_text_filters_and_projection() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("qmodes");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    // Dense vector search across all three metrics. With one-hot vectors,
    // every metric ranks the matching axis first.
    for metric in ["cosine", "dot", "euclidean"] {
        let ids = srv
            .top_ids(&ns, queries::vector(&basis(DIM, 5), metric, 3))
            .await
            .unwrap();
        assert_eq!(
            ids.first().map(String::as_str),
            Some("doc-5"),
            "metric {metric} should rank doc-5 first, got {ids:?}"
        );
    }

    // BM25 full-text: unique token wins; shared token returns everything.
    let ids = srv.top_ids(&ns, queries::text("alpha3", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-3"));
    let ids = srv.top_ids(&ns, queries::text("common", 20)).await.unwrap();
    assert_eq!(ids.len(), 8, "every document contains 'common'");

    // top_k is respected exactly.
    let ids = srv.top_ids(&ns, queries::text("common", 3)).await.unwrap();
    assert_eq!(ids.len(), 3, "top_k=3 must yield exactly 3 hits from 8 matches");

    // eq filter on vector search.
    let q = queries::with_filter(
        queries::vector(&basis(DIM, 4), "cosine", 1),
        filters::eq("lang", json!("en")),
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-4"), "doc-4 is lang=en");

    // eq filter that excludes the nearest neighbour.
    let q = queries::with_filter(
        queries::vector(&basis(DIM, 4), "cosine", 1),
        filters::eq("lang", json!("fr")),
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    let top = ids.first().cloned().unwrap_or_default();
    assert_ne!(top, "doc-4");
    let idx: usize = top.trim_start_matches("doc-").parse().expect("doc-N id");
    assert_eq!(idx % 2, 1, "lang=fr means odd index, got {top}");

    // in filter.
    let q = queries::with_filter(
        queries::text("common", 20),
        filters::r#in("lang", json!(["fr"])),
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    assert_eq!(ids.len(), 4);
    for id in &ids {
        let idx: usize = id.trim_start_matches("doc-").parse().unwrap();
        assert_eq!(idx % 2, 1, "in[fr] must only return odd-index docs, got {id}");
    }

    // gt filter on a numeric attribute.
    let q = queries::with_filter(queries::text("common", 20), filters::gt("rank", json!(5)));
    let ids = srv.top_ids(&ns, q).await.unwrap();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["doc-6".to_string(), "doc-7".to_string()]);

    // and() composition.
    let q = queries::with_filter(
        queries::text("common", 20),
        filters::and(vec![
            filters::gte("rank", json!(2)),
            filters::lt("rank", json!(5)),
            filters::eq("lang", json!("en")),
        ]),
    );
    let mut ids = srv.top_ids(&ns, q).await.unwrap();
    ids.sort();
    assert_eq!(ids, vec!["doc-2".to_string(), "doc-4".to_string()]);

    // not() composition.
    let q = queries::with_filter(
        queries::text("common", 20),
        filters::not(filters::eq("lang", json!("en"))),
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    assert_eq!(ids.len(), 4);

    // Attribute projection: only requested attributes come back.
    let q = queries::with_include(queries::vector(&basis(DIM, 0), "cosine", 1), &["title"]);
    let resp = srv.query(&ns, q).await.unwrap();
    let hits = extract_hits(&resp);
    assert!(!hits.is_empty());
    let h = &hits[0];
    assert!(
        doc_attr(h, "title").is_some(),
        "projected attribute 'title' must be present: {h}"
    );
    assert!(
        doc_attr(h, "body").is_none(),
        "unprojected attribute 'body' must be absent: {h}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_queries_rrf_and_weighted() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("hybrid");
    seed(&srv, &ns, 8).await.unwrap();
    srv.try_compact(&ns).await;

    // RRF: vector leg points at doc-2, text leg at doc-6; both must surface
    // in the fused top-2.
    let q = queries::hybrid(&basis(DIM, 2), "cosine", "alpha6", queries::rrf(), 5);
    let ids = srv.top_ids(&ns, q).await.unwrap();
    let top2: Vec<&str> = ids.iter().take(2).map(String::as_str).collect();
    assert!(
        top2.contains(&"doc-2") && top2.contains(&"doc-6"),
        "RRF top-2 should contain both legs' winners, got {ids:?}"
    );

    // Weighted fusion, vector-only weight: vector winner must rank first.
    let q = queries::hybrid(
        &basis(DIM, 2),
        "cosine",
        "alpha6",
        queries::weighted(1.0, 0.0),
        5,
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-2"));

    // Weighted fusion, text-only weight: text winner must rank first.
    let q = queries::hybrid(
        &basis(DIM, 2),
        "cosine",
        "alpha6",
        queries::weighted(0.0, 1.0),
        5,
    );
    let ids = srv.top_ids(&ns, q).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-6"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_namespace_is_fully_independent() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("copy-src");
    let c = ns_name("copy-dst");

    seed(&srv, &a, 5).await.unwrap();
    srv.copy(&a, &c).await.unwrap();
    assert_ids(&srv, &c, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4"])
        .await
        .unwrap();

    // Mutations on either side stay on that side.
    srv.upsert(&a, &[doc("only-a", basis(DIM, 6), "OnlyA", "only in a", json!({}))])
        .await
        .unwrap();
    srv.upsert(&c, &[doc("only-c", basis(DIM, 7), "OnlyC", "only in c", json!({}))])
        .await
        .unwrap();

    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "only-a"])
        .await
        .unwrap();
    assert_ids(&srv, &c, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "only-c"])
        .await
        .unwrap();

    // Deleting the source must not break the copy.
    srv.delete_namespace(&a).await.unwrap();
    let ids = srv
        .top_ids(&c, queries::vector(&basis(DIM, 3), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-3"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn error_handling_for_bad_requests() {
    let srv = TestServer::spawn_local().await.unwrap();
    let ns = ns_name("errors");
    seed(&srv, &ns, 2).await.unwrap();

    // Querying a missing namespace.
    let (status, _) = srv
        .post(
            &api::query(&ns_name("ghost")),
            Some(READER_KEY),
            queries::text("x", 1),
        )
        .await;
    assert_eq!(status, 404);

    // Upserting into a missing namespace.
    let (status, _) = srv
        .post(
            &api::documents(&ns_name("ghost")),
            Some(WRITER_KEY),
            json!({"documents": corpus(1)}),
        )
        .await;
    assert_eq!(status, 404);

    // Malformed filter must be a client error, not a 5xx.
    let (status, _) = srv
        .post(
            &api::query(&ns),
            Some(READER_KEY),
            json!({"top_k": 1, "text": {"query": "x"}, "filter": {"op": "no-such-op"}}),
        )
        .await;
    assert!(
        (400..500).contains(&(status as i32)),
        "malformed filter should be 4xx, got {status}"
    );

    // Document without an ID must be rejected.
    let (status, _) = srv
        .post(
            &api::documents(&ns),
            Some(WRITER_KEY),
            json!({"documents": [{"vector": basis(DIM, 0), "attributes": {"title": "no id"}}]}),
        )
        .await;
    assert!(
        (400..500).contains(&(status as i32)),
        "document without id should be 4xx, got {status}"
    );

    // Branching onto an existing namespace name must be rejected.
    let other = ns_name("exists");
    srv.create_namespace(&other).await.unwrap();
    let (status, _) = srv
        .post(&api::branch(&ns), Some(ADMIN_KEY), json!({"target": &other}))
        .await;
    assert!(
        status == 409 || status == 400,
        "branching onto an existing namespace should be 409/400, got {status}"
    );
}
