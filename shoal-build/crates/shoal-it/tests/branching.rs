//! Copy-on-write branching tests: isolation in both directions, multi-level
//! branches, refcount safety on deletion, and query correctness on branches.

use serde_json::json;
use shoal_it::{
    assert_ids, basis, doc, doc_attr, ns_name, queries, seed, TestServer, DIM,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn branch_sees_source_data_at_branch_point() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("src");
    let b = ns_name("br");

    seed(&srv, &a, 6).await.unwrap();
    srv.try_compact(&a).await; // exercise branching over compacted segments too
    srv.branch(&a, &b).await.unwrap();

    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-5"])
        .await
        .unwrap();

    // Spot-check content came through, not just IDs.
    let docs = srv.export_docs(&b).await.unwrap();
    let d0 = docs
        .iter()
        .find(|d| shoal_it::hit_id(d).as_deref() == Some("doc-0"))
        .expect("doc-0 present in branch");
    assert_eq!(
        doc_attr(d0, "title").and_then(|v| v.as_str()),
        Some("Title 0"),
        "branch must carry source attributes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_to_source_do_not_affect_branch() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("src");
    let b = ns_name("br");

    seed(&srv, &a, 4).await.unwrap();
    srv.branch(&a, &b).await.unwrap();

    // Mutate the source: one new doc, one overwrite.
    srv.upsert(
        &a,
        &[
            doc("doc-100", basis(DIM, 7), "New In Source", "source only zulu", json!({})),
            doc("doc-0", basis(DIM, 0), "Rewritten Title", "rewritten body", json!({})),
        ],
    )
    .await
    .unwrap();

    // Source reflects the mutation.
    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-100"])
        .await
        .unwrap();

    // Branch is frozen at branch point.
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3"])
        .await
        .unwrap();
    let docs = srv.export_docs(&b).await.unwrap();
    let d0 = docs
        .iter()
        .find(|d| shoal_it::hit_id(d).as_deref() == Some("doc-0"))
        .unwrap();
    assert_eq!(
        doc_attr(d0, "title").and_then(|v| v.as_str()),
        Some("Title 0"),
        "overwriting doc-0 in source must not leak into the branch"
    );

    // Text added to the source must not be findable on the branch.
    let ids = srv.top_ids(&b, queries::text("zulu", 5)).await.unwrap();
    assert!(
        !ids.contains(&"doc-100".to_string()),
        "source-only document leaked into branch text search: {ids:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_to_branch_do_not_affect_source() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("src");
    let b = ns_name("br");

    seed(&srv, &a, 4).await.unwrap();
    srv.branch(&a, &b).await.unwrap();

    srv.upsert(
        &b,
        &[
            doc("doc-200", basis(DIM, 6), "Branch Only", "branch only yankee", json!({})),
            doc("doc-1", basis(DIM, 1), "Branch Edit", "edited on branch", json!({})),
        ],
    )
    .await
    .unwrap();
    srv.delete_by_ids(&b, &["doc-3"]).await.unwrap();

    // Branch reflects its own writes and deletes.
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-200"])
        .await
        .unwrap();

    // Source is untouched: same ids, original doc-1.
    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3"])
        .await
        .unwrap();
    let docs = srv.export_docs(&a).await.unwrap();
    let d1 = docs
        .iter()
        .find(|d| shoal_it::hit_id(d).as_deref() == Some("doc-1"))
        .unwrap();
    assert_eq!(
        doc_attr(d1, "title").and_then(|v| v.as_str()),
        Some("Title 1"),
        "branch edit of doc-1 must not leak into source"
    );

    let ids = srv.top_ids(&a, queries::text("yankee", 5)).await.unwrap();
    assert!(
        !ids.contains(&"doc-200".to_string()),
        "branch-only document leaked into source text search: {ids:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_level_branches_are_isolated() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("lvl-a");
    let b = ns_name("lvl-b");
    let c = ns_name("lvl-c");

    seed(&srv, &a, 4).await.unwrap();
    srv.branch(&a, &b).await.unwrap();
    srv.upsert(&b, &[doc("doc-b", basis(DIM, 5), "B Doc", "level b xray", json!({}))])
        .await
        .unwrap();

    srv.branch(&b, &c).await.unwrap();
    srv.upsert(&c, &[doc("doc-c", basis(DIM, 6), "C Doc", "level c whiskey", json!({}))])
        .await
        .unwrap();

    // C inherits A's corpus and B's addition, plus its own.
    assert_ids(&srv, &c, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-b", "doc-c"])
        .await
        .unwrap();
    // B does not see C's writes.
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-b"])
        .await
        .unwrap();
    // A sees neither.
    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3"])
        .await
        .unwrap();

    // Late write to the root must not propagate down the chain.
    srv.upsert(&a, &[doc("doc-a2", basis(DIM, 7), "A Late", "late alpha-late", json!({}))])
        .await
        .unwrap();
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-b"])
        .await
        .unwrap();
    assert_ids(&srv, &c, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-b", "doc-c"])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_branch_keeps_source_intact() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("keep-src");
    let b = ns_name("doomed-br");

    seed(&srv, &a, 5).await.unwrap();
    srv.try_compact(&a).await;
    srv.branch(&a, &b).await.unwrap();
    srv.upsert(&b, &[doc("doc-bx", basis(DIM, 6), "BX", "bx body", json!({}))])
        .await
        .unwrap();

    srv.delete_namespace(&b).await.unwrap();

    // The branch is gone…
    let (status, _) = srv.get(&shoal_it::api::namespace(&b), Some(shoal_it::ADMIN_KEY)).await;
    assert_eq!(status, 404, "deleted branch should 404");

    // …and the source must remain fully readable and queryable: refcounting
    // must have prevented deletion of shared segments.
    assert_ids(&srv, &a, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4"])
        .await
        .unwrap();
    let ids = srv
        .top_ids(&a, queries::vector(&basis(DIM, 2), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-2"));
    let ids = srv.top_ids(&a, queries::text("alpha3", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-3"));

    // Re-branching from the source still works after the delete.
    let b2 = ns_name("rebranch");
    srv.branch(&a, &b2).await.unwrap();
    assert_ids(&srv, &b2, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4"])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_source_keeps_branch_queryable() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("doomed-src");
    let b = ns_name("keep-br");

    seed(&srv, &a, 5).await.unwrap();
    srv.try_compact(&a).await;
    srv.branch(&a, &b).await.unwrap();

    srv.delete_namespace(&a).await.unwrap();

    // The branch still references the (shared, immutable) source segments;
    // refcounting must keep them alive past the source-namespace deletion.
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4"])
        .await
        .unwrap();

    let ids = srv
        .top_ids(&b, queries::vector(&basis(DIM, 4), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-4"));

    let ids = srv.top_ids(&b, queries::text("alpha1", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-1"));

    // The branch must also remain writable after losing its parent.
    srv.upsert(&b, &[doc("doc-after", basis(DIM, 7), "After", "after orphaning", json!({}))])
        .await
        .unwrap();
    assert_ids(&srv, &b, &["doc-0", "doc-1", "doc-2", "doc-3", "doc-4", "doc-after"])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn querying_branches_vector_text_and_filters() {
    let srv = TestServer::spawn_local().await.unwrap();
    let a = ns_name("q-src");
    let b = ns_name("q-br");

    seed(&srv, &a, 8).await.unwrap();
    srv.try_compact(&a).await;
    srv.branch(&a, &b).await.unwrap();

    // Vector query on the branch.
    let ids = srv
        .top_ids(&b, queries::vector(&basis(DIM, 3), "cosine", 1))
        .await
        .unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-3"));

    // Full-text query on the branch.
    let ids = srv.top_ids(&b, queries::text("alpha5", 3)).await.unwrap();
    assert_eq!(ids.first().map(String::as_str), Some("doc-5"));

    // Filtered vector query on the branch: doc-2 has lang=en; restricting to
    // fr must surface a different (odd-index) document.
    let q = queries::with_filter(
        queries::vector(&basis(DIM, 2), "cosine", 1),
        shoal_it::filters::eq("lang", json!("fr")),
    );
    let ids = srv.top_ids(&b, q).await.unwrap();
    let top = ids.first().cloned().unwrap_or_default();
    assert_ne!(top, "doc-2", "filter must exclude doc-2 (lang=en)");
    let idx: usize = top.trim_start_matches("doc-").parse().expect("doc-N id");
    assert_eq!(idx % 2, 1, "lang=fr corresponds to odd indices, got {top}");
}
