//! Write-path contract tests: batched upserts, patch, delete-by-id,
//! overwrite semantics, idempotency keys, conditional writes, and the
//! row- vs column-oriented ingestion paths.
//!
//! Every test runs against the in-memory and local-filesystem object stores.

mod common;

use std::collections::BTreeMap;

use common::*;
use reef_types::{ColumnarBatch, DocId, Document, Patch, WriteCondition};

// ---------------------------------------------------------------------------
// Basic upsert + read-your-writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_is_readable_before_any_flush() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-ryw").await;
        engine
            .write(upsert_batch(vec![doc_attrs("a", &[("k", s("v"))])]))
            .await
            .unwrap();

        let got = engine
            .get(&DocId::from("a"))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("doc 'a' missing on backend {}", ts.name));
        assert_eq!(got.attributes.get("k"), Some(&s("v")), "backend={}", ts.name);
        assert_eq!(doc_count(&engine).await, 1, "backend={}", ts.name);
    }
}

#[tokio::test]
async fn upsert_is_readable_after_flush_to_segment() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-flush").await;
        engine
            .write(upsert_batch(numbered_docs("d", 40)))
            .await
            .unwrap();
        engine.flush().await.unwrap();

        // At least one immutable segment must now be referenced by the manifest.
        let manifest = engine.manifest_snapshot().await.unwrap();
        assert!(
            !manifest.segments.is_empty(),
            "flush should produce a segment, backend={}",
            ts.name
        );

        assert_eq!(doc_count(&engine).await, 40, "backend={}", ts.name);
        let got = engine
            .get(&DocId::from("d00017"))
            .await
            .unwrap()
            .expect("d00017 after flush");
        assert_eq!(got.attributes.get("n"), Some(&i(17)));
    }
}

#[tokio::test]
async fn many_batched_upserts_accumulate_correctly() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-batches").await;

        // Six batches of 50; flush halfway through so the result spans both
        // segment files and unflushed WAL state.
        for batch_no in 0..6usize {
            let docs: Vec<Document> = (0..50usize)
                .map(|j| {
                    let idx = batch_no * 50 + j;
                    doc_attrs(
                        &format!("m{idx:05}"),
                        &[("n", i(idx as i64)), ("batch", i(batch_no as i64))],
                    )
                })
                .collect();
            engine.write(upsert_batch(docs)).await.unwrap();
            if batch_no == 2 {
                engine.flush().await.unwrap();
            }
        }

        assert_eq!(doc_count(&engine).await, 300, "backend={}", ts.name);
        assert_unique_ids(&engine).await;

        // Spot-check one doc from a flushed batch and one from an unflushed batch.
        let early = engine.get(&DocId::from("m00010")).await.unwrap().unwrap();
        assert_eq!(early.attributes.get("batch"), Some(&i(0)));
        let late = engine.get(&DocId::from("m00299")).await.unwrap().unwrap();
        assert_eq!(late.attributes.get("batch"), Some(&i(5)));
    }
}

// ---------------------------------------------------------------------------
// Overwrite (last-writer-wins per document id)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upsert_same_id_overwrites_and_never_duplicates() {
    for ts in all_backends() {
        let ns = "wp-overwrite";
        let engine = open_engine(&ts.store, ns).await;

        engine
            .write(upsert_batch(vec![doc_attrs("a", &[("rev", i(1))])]))
            .await
            .unwrap();
        engine.flush().await.unwrap();

        // Overwrite the now-segment-resident version from the WAL/memtable.
        engine
            .write(upsert_batch(vec![doc_attrs("a", &[("rev", i(2))])]))
            .await
            .unwrap();

        let check = |d: Document, stage: &str, backend: &str| {
            assert_eq!(
                d.attributes.get("rev"),
                Some(&i(2)),
                "stage={stage} backend={backend}"
            );
        };

        check(
            engine.get(&DocId::from("a")).await.unwrap().unwrap(),
            "pre-flush",
            ts.name,
        );
        assert_eq!(doc_count(&engine).await, 1);

        engine.flush().await.unwrap();
        check(
            engine.get(&DocId::from("a")).await.unwrap().unwrap(),
            "post-flush",
            ts.name,
        );
        assert_eq!(doc_count(&engine).await, 1);

        // Compaction must collapse both versions into one and keep the newest.
        engine.compact().await.unwrap();
        check(
            engine.get(&DocId::from("a")).await.unwrap().unwrap(),
            "post-compact",
            ts.name,
        );
        assert_eq!(doc_count(&engine).await, 1);
        assert_unique_ids(&engine).await;

        // And the winner must survive a restart.
        drop(engine);
        let engine = open_engine(&ts.store, ns).await;
        check(
            engine.get(&DocId::from("a")).await.unwrap().unwrap(),
            "post-restart",
            ts.name,
        );
    }
}

// ---------------------------------------------------------------------------
// Patch: set / unset / vector replacement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_set_unset_and_vector_replacement() {
    for ts in all_backends() {
        let ns = "wp-patch";
        let engine = open_engine(&ts.store, ns).await;

        engine
            .write(upsert_batch(vec![doc_vec(
                "p1",
                vec![1.0, 0.0],
                &[("a", i(1)), ("b", s("x"))],
            )]))
            .await
            .unwrap();
        engine.flush().await.unwrap();

        let patch = Patch {
            id: DocId::from("p1"),
            set: BTreeMap::from([
                ("b".to_string(), s("y")),
                ("c".to_string(), b(true)),
            ]),
            unset: vec!["a".to_string()],
            vector: Some(vec![0.0, 1.0]),
        };
        engine.write(patch_batch(vec![patch])).await.unwrap();

        let check = |d: Document, stage: &str| {
            assert_eq!(d.attributes.get("b"), Some(&s("y")), "stage={stage}");
            assert_eq!(d.attributes.get("c"), Some(&b(true)), "stage={stage}");
            assert!(
                d.attributes.get("a").is_none(),
                "'a' should be unset, stage={stage}"
            );
            assert_eq!(d.vector, Some(vec![0.0, 1.0]), "stage={stage}");
        };

        // Visible immediately (patch applied over the segment-resident base).
        check(
            engine.get(&DocId::from("p1")).await.unwrap().expect("p1"),
            "pre-flush",
        );

        // Patch must survive flush, restart, and compaction.
        engine.flush().await.unwrap();
        drop(engine);
        let engine = open_engine(&ts.store, ns).await;
        check(
            engine
                .get(&DocId::from("p1"))
                .await
                .unwrap()
                .expect("p1 after restart"),
            "post-restart",
        );
        engine.compact().await.unwrap();
        check(
            engine
                .get(&DocId::from("p1"))
                .await
                .unwrap()
                .expect("p1 after compact"),
            "post-compact",
        );
        assert_eq!(doc_count(&engine).await, 1, "backend={}", ts.name);
    }
}

#[tokio::test]
async fn patch_applies_to_unflushed_document_too() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-patch-hot").await;
        engine
            .write(upsert_batch(vec![doc_attrs("h1", &[("v", i(1))])]))
            .await
            .unwrap();
        // No flush in between: patch must fold onto the memtable version.
        engine
            .write(patch_batch(vec![Patch {
                id: DocId::from("h1"),
                set: BTreeMap::from([("v".to_string(), i(2))]),
                unset: vec![],
                vector: None,
            }]))
            .await
            .unwrap();

        let d = engine.get(&DocId::from("h1")).await.unwrap().unwrap();
        assert_eq!(d.attributes.get("v"), Some(&i(2)), "backend={}", ts.name);
        assert_eq!(doc_count(&engine).await, 1);
    }
}

// ---------------------------------------------------------------------------
// Delete by id (tombstones across memtable, segments, compaction, restart)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_by_id_removes_segment_resident_documents() {
    for ts in all_backends() {
        let ns = "wp-del-seg";
        let engine = open_engine(&ts.store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("del", 5)))
            .await
            .unwrap();
        engine.flush().await.unwrap();

        engine
            .write(delete_batch(&["del00001", "del00003"]))
            .await
            .unwrap();

        assert_eq!(doc_count(&engine).await, 3, "backend={}", ts.name);
        assert!(engine.get(&DocId::from("del00001")).await.unwrap().is_none());
        assert!(engine.get(&DocId::from("del00003")).await.unwrap().is_none());
        assert!(engine.get(&DocId::from("del00002")).await.unwrap().is_some());

        // Tombstones must persist through flush + restart + compaction.
        engine.flush().await.unwrap();
        drop(engine);
        let engine = open_engine(&ts.store, ns).await;
        assert_eq!(doc_count(&engine).await, 3, "after restart, backend={}", ts.name);
        assert!(engine.get(&DocId::from("del00001")).await.unwrap().is_none());

        engine.compact().await.unwrap();
        assert_eq!(doc_count(&engine).await, 3, "after compact, backend={}", ts.name);
        assert_eq!(
            scan_ids(&engine).await,
            vec!["del00000", "del00002", "del00004"]
        );
    }
}

#[tokio::test]
async fn delete_by_id_removes_unflushed_documents() {
    for ts in all_backends() {
        let ns = "wp-del-hot";
        let engine = open_engine(&ts.store, ns).await;
        engine
            .write(upsert_batch(numbered_docs("hot", 3)))
            .await
            .unwrap();
        engine.write(delete_batch(&["hot00000"])).await.unwrap();

        assert_eq!(doc_count(&engine).await, 2, "backend={}", ts.name);
        assert!(engine.get(&DocId::from("hot00000")).await.unwrap().is_none());

        // Restart with nothing flushed: WAL replay must reproduce the delete.
        drop(engine);
        let engine = open_engine(&ts.store, ns).await;
        assert_eq!(doc_count(&engine).await, 2, "after restart, backend={}", ts.name);
        assert!(engine.get(&DocId::from("hot00000")).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn deleting_unknown_id_is_a_noop() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-del-unknown").await;
        engine
            .write(upsert_batch(vec![doc("only")]))
            .await
            .unwrap();
        engine
            .write(delete_batch(&["does-not-exist"]))
            .await
            .unwrap();
        assert_eq!(doc_count(&engine).await, 1, "backend={}", ts.name);
        assert!(engine.get(&DocId::from("only")).await.unwrap().is_some());
    }
}

// ---------------------------------------------------------------------------
// Idempotency keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idempotency_key_deduplicates_retried_batches() {
    for ts in all_backends() {
        let ns = "wp-idem";
        let engine = open_engine(&ts.store, ns).await;

        let mut batch = upsert_batch(numbered_docs("k", 10));
        batch.idempotency_key = Some("ingest-job-42".to_string());

        let r1 = engine.write(batch.clone()).await.unwrap();
        assert!(r1.applied, "first write must apply, backend={}", ts.name);

        // A client retry with the same key must be acknowledged but not re-applied.
        let r2 = engine.write(batch.clone()).await.unwrap();
        assert!(
            !r2.applied,
            "duplicate idempotency key must not re-apply, backend={}",
            ts.name
        );
        assert_eq!(doc_count(&engine).await, 10);

        // Idempotency must survive a restart: the dedup state is recovered
        // from the WAL, not held only in process memory.
        drop(engine);
        let engine = open_engine(&ts.store, ns).await;
        let r3 = engine.write(batch).await.unwrap();
        assert!(
            !r3.applied,
            "duplicate key after restart must not re-apply, backend={}",
            ts.name
        );
        assert_eq!(doc_count(&engine).await, 10);
        assert_unique_ids(&engine).await;
    }
}

#[tokio::test]
async fn distinct_idempotency_keys_apply_independently() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-idem-distinct").await;

        let mut b1 = upsert_batch(vec![doc("x1")]);
        b1.idempotency_key = Some("job-a".to_string());
        let mut b2 = upsert_batch(vec![doc("x2")]);
        b2.idempotency_key = Some("job-b".to_string());

        assert!(engine.write(b1).await.unwrap().applied);
        assert!(engine.write(b2).await.unwrap().applied);
        assert_eq!(doc_count(&engine).await, 2, "backend={}", ts.name);
    }
}

// ---------------------------------------------------------------------------
// Conditional writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conditional_write_on_last_sequence_succeeds_then_rejects_stale() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-cond-seq").await;

        let r1 = engine
            .write(upsert_batch(vec![doc_attrs("a", &[("k", s("v1"))])]))
            .await
            .unwrap();

        // Compare-and-swap against the sequence we just observed: succeeds.
        let mut cas = upsert_batch(vec![doc_attrs("a", &[("k", s("v2"))])]);
        cas.condition = Some(WriteCondition::LastSeqEquals(r1.seq));
        let r2 = engine.write(cas).await.unwrap();
        assert!(r2.applied);

        // The same precondition is now stale and must be rejected without
        // mutating anything.
        let mut stale = upsert_batch(vec![doc_attrs("a", &[("k", s("LOST-UPDATE"))])]);
        stale.condition = Some(WriteCondition::LastSeqEquals(r1.seq));
        assert!(
            engine.write(stale).await.is_err(),
            "stale precondition must fail, backend={}",
            ts.name
        );

        let d = engine.get(&DocId::from("a")).await.unwrap().unwrap();
        assert_eq!(
            d.attributes.get("k"),
            Some(&s("v2")),
            "rejected write must leave state untouched, backend={}",
            ts.name
        );
    }
}

#[tokio::test]
async fn conditional_all_new_rejects_existing_ids_atomically() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-cond-new").await;
        engine
            .write(upsert_batch(vec![doc("exists")]))
            .await
            .unwrap();

        // One id collides, one is new — the whole batch must be rejected.
        let mut batch = upsert_batch(vec![doc("exists"), doc("brand-new")]);
        batch.condition = Some(WriteCondition::AllNew);
        assert!(
            engine.write(batch).await.is_err(),
            "AllNew must reject when any id exists, backend={}",
            ts.name
        );
        assert!(
            engine.get(&DocId::from("brand-new")).await.unwrap().is_none(),
            "rejected conditional batch must be all-or-nothing, backend={}",
            ts.name
        );

        // With only fresh ids the same condition passes.
        let mut ok = upsert_batch(vec![doc("brand-new")]);
        ok.condition = Some(WriteCondition::AllNew);
        assert!(engine.write(ok).await.unwrap().applied);
        assert_eq!(doc_count(&engine).await, 2);
    }
}

// ---------------------------------------------------------------------------
// Columnar ingestion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn columnar_ingestion_matches_row_ingestion_exactly() {
    for ts in all_backends() {
        let row_engine = open_engine(&ts.store, "wp-col-row").await;
        let col_engine = open_engine(&ts.store, "wp-col-col").await;

        let n = 8usize;
        let tag = |idx: usize| if idx % 2 == 0 { "even" } else { "odd" };

        // Row-oriented ingestion of the reference data set.
        let row_docs: Vec<Document> = (0..n)
            .map(|idx| {
                doc_vec(
                    &format!("c{idx:03}"),
                    vec![idx as f32, 1.0 - idx as f32],
                    &[("n", i(idx as i64)), ("tag", s(tag(idx)))],
                )
            })
            .collect();
        row_engine.write(upsert_batch(row_docs)).await.unwrap();

        // Column-oriented ingestion of the same data set.
        let columnar = ColumnarBatch {
            ids: (0..n).map(|idx| format!("c{idx:03}")).collect(),
            vectors: Some((0..n).map(|idx| vec![idx as f32, 1.0 - idx as f32]).collect()),
            attributes: BTreeMap::from([
                (
                    "n".to_string(),
                    (0..n).map(|idx| i(idx as i64)).collect::<Vec<_>>(),
                ),
                (
                    "tag".to_string(),
                    (0..n).map(|idx| s(tag(idx))).collect::<Vec<_>>(),
                ),
            ]),
        };
        col_engine.write_columnar(columnar).await.unwrap();

        // Identical before flush...
        assert_eq!(
            snapshot(&row_engine).await,
            snapshot(&col_engine).await,
            "pre-flush, backend={}",
            ts.name
        );

        // ...and identical after both have been folded into segments and restarted.
        row_engine.flush().await.unwrap();
        col_engine.flush().await.unwrap();
        drop(row_engine);
        drop(col_engine);
        let row_engine = open_engine(&ts.store, "wp-col-row").await;
        let col_engine = open_engine(&ts.store, "wp-col-col").await;
        assert_eq!(
            snapshot(&row_engine).await,
            snapshot(&col_engine).await,
            "post-restart, backend={}",
            ts.name
        );
    }
}

#[tokio::test]
async fn columnar_batch_rejects_mismatched_column_lengths() {
    for ts in all_backends() {
        let engine = open_engine(&ts.store, "wp-col-bad").await;
        let bad = ColumnarBatch {
            ids: vec!["a".to_string(), "b".to_string()],
            vectors: None,
            // Only one value for two ids: must be rejected up front.
            attributes: BTreeMap::from([("n".to_string(), vec![i(1)])]),
        };
        assert!(
            engine.write_columnar(bad).await.is_err(),
            "ragged columnar batch must be rejected, backend={}",
            ts.name
        );
        assert_eq!(doc_count(&engine).await, 0);
    }
}
