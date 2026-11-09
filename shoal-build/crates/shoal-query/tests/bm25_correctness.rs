//! BM25 correctness tests: the engine's scores are checked against an
//! independently written reference implementation of the documented formula,
//! and the classic BM25 behaviors (IDF ordering, TF saturation, length
//! normalization, field boosting) are verified directly.

use shoal_query::text::bm25::{search, Bm25Params};
use shoal_query::text::inverted::{InvertedIndexBuilder, InvertedSegment, TextError};
use shoal_query::text::tokenizer::Tokenizer;

// ---------------------------------------------------------------------------
// Reference implementation (written independently of the engine code)
// ---------------------------------------------------------------------------

fn ref_idf(n: u32, df: u32) -> f32 {
    (1.0 + (n as f32 - df as f32 + 0.5) / (df as f32 + 0.5)).ln()
}

fn ref_tf_norm(tf: u32, dl: u32, avgdl: f32, k1: f32, b: f32) -> f32 {
    let tf = tf as f32;
    tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl as f32 / avgdl))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_segment(fields: &[&str], docs: &[(u32, Vec<(&str, &str)>)]) -> InvertedSegment {
    let mut b = InvertedIndexBuilder::new(Tokenizer::default(), fields).unwrap();
    for (doc, fs) in docs {
        b.add_document(*doc, fs).unwrap();
    }
    b.build()
}

fn score_of(results: &[shoal_query::text::bm25::ScoredDoc], doc: u32) -> f32 {
    results
        .iter()
        .find(|r| r.doc == doc)
        .unwrap_or_else(|| panic!("doc {doc} missing from results"))
        .score
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn scores_match_reference_formula() {
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "rust database storage")]),
            (1, vec![("body", "rust search engine")]),
            (2, vec![("body", "database engine engine fast")]),
        ],
    );
    let tok = Tokenizer::default();
    let params = Bm25Params::default();
    let body = seg.field_id("body").unwrap();
    let n = seg.field_stats(body).unwrap().doc_count;
    let avgdl = seg.avg_field_len(body);
    assert_eq!(n, 3);
    assert!((avgdl - 10.0 / 3.0).abs() < 1e-6);

    let results = search(&seg, &tok, "engine database", &[], &params, 10, None).unwrap();

    // Reference: sum per query term of idf * tfnorm, qtf = 1 each.
    for doc in [0u32, 1, 2] {
        let mut expected = 0.0f32;
        for term in ["database", "engine"] {
            let df = seg.doc_freq(term, body);
            if df == 0 {
                continue;
            }
            // tf for this doc/term from the postings list.
            let tf = seg
                .postings(term, body)
                .unwrap()
                .find(|&(d, _)| d == doc)
                .map(|(_, tf)| tf)
                .unwrap_or(0);
            if tf == 0 {
                continue;
            }
            let dl = seg.doc_len(body, doc);
            expected += ref_idf(n, df) * ref_tf_norm(tf, dl, avgdl, params.k1, params.b);
        }
        if expected > 0.0 {
            let got = score_of(&results, doc);
            assert!(
                (got - expected).abs() < 1e-5,
                "doc {doc}: engine={got}, reference={expected}"
            );
        } else {
            assert!(results.iter().all(|r| r.doc != doc));
        }
    }

    // Sorted descending, deterministic.
    for w in results.windows(2) {
        assert!(w[0].score >= w[1].score);
    }
}

#[test]
fn rare_terms_outscore_common_terms() {
    // doc 0 contains both "rare" and "common" once each, same field length.
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "rare common")]),
            (1, vec![("body", "common filler1")]),
            (2, vec![("body", "common filler2")]),
            (3, vec![("body", "common filler3")]),
        ],
    );
    let tok = Tokenizer::default();
    let params = Bm25Params::default();
    let rare = search(&seg, &tok, "rare", &[], &params, 10, None).unwrap();
    let common = search(&seg, &tok, "common", &[], &params, 10, None).unwrap();
    let rare_score = score_of(&rare, 0);
    let common_score = score_of(&common, 0);
    assert!(
        rare_score > common_score,
        "rare={rare_score} should beat common={common_score}"
    );
}

#[test]
fn term_frequency_saturates() {
    // Same field length (8 tokens), tf = 1, 2, 3.
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "cat f1 f2 f3 f4 f5 f6 f7")]),
            (1, vec![("body", "cat cat f1 f2 f3 f4 f5 f6")]),
            (2, vec![("body", "cat cat cat f1 f2 f3 f4 f5")]),
        ],
    );
    let tok = Tokenizer::default();
    let results = search(&seg, &tok, "cat", &[], &Bm25Params::default(), 10, None).unwrap();
    let s1 = score_of(&results, 0);
    let s2 = score_of(&results, 1);
    let s3 = score_of(&results, 2);
    assert!(s1 < s2 && s2 < s3, "scores must grow with tf");
    assert!(
        (s2 - s1) > (s3 - s2),
        "tf gains must diminish (saturation): {} vs {}",
        s2 - s1,
        s3 - s2
    );
}

#[test]
fn shorter_documents_score_higher_for_equal_tf() {
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "apple")]),
            (1, vec![("body", "apple banana cherry date egg fig grape")]),
        ],
    );
    let tok = Tokenizer::default();
    let results = search(&seg, &tok, "apple", &[], &Bm25Params::default(), 10, None).unwrap();
    assert!(score_of(&results, 0) > score_of(&results, 1));
    assert_eq!(results[0].doc, 0);
}

#[test]
fn field_boosting_changes_ranking() {
    let seg = build_segment(
        &["title", "body"],
        &[
            (0, vec![("title", "rust guide"), ("body", "an introduction")]),
            (
                1,
                vec![("title", "cooking"), ("body", "rust rust rust in pipes")],
            ),
        ],
    );
    let tok = Tokenizer::default();
    let params = Bm25Params::default();

    // Heavy title boost: doc 0 (title match) wins.
    let boosts = [("title".to_string(), 5.0), ("body".to_string(), 1.0)];
    let results = search(&seg, &tok, "rust", &boosts, &params, 10, None).unwrap();
    assert_eq!(results[0].doc, 0, "title boost should promote doc 0");

    // Body only: doc 1 (high body tf) wins.
    let boosts = [("body".to_string(), 1.0)];
    let results = search(&seg, &tok, "rust", &boosts, &params, 10, None).unwrap();
    assert_eq!(results[0].doc, 1, "body-only search should promote doc 1");
    assert!(results.iter().all(|r| r.doc != 0) || score_of(&results, 1) > score_of(&results, 0));

    // Unknown boost field is an error.
    let boosts = [("nope".to_string(), 1.0)];
    let err = search(&seg, &tok, "rust", &boosts, &params, 10, None).unwrap_err();
    assert!(matches!(err, TextError::UnknownField(_)));
}

#[test]
fn multi_term_queries_prefer_docs_matching_more_terms() {
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "vector database for search")]),
            (1, vec![("body", "vector graphics editor tool")]),
            (2, vec![("body", "relational database engine here")]),
        ],
    );
    let tok = Tokenizer::default();
    let results = search(
        &seg,
        &tok,
        "vector database",
        &[],
        &Bm25Params::default(),
        10,
        None,
    )
    .unwrap();
    assert_eq!(results[0].doc, 0, "doc matching both terms should rank first");
    assert_eq!(results.len(), 3);
}

#[test]
fn repeated_query_terms_increase_weight() {
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "alpha beta gamma delta")]),
            (1, vec![("body", "beta beta epsilon zeta")]),
        ],
    );
    let tok = Tokenizer::default();
    let params = Bm25Params::default();
    let single = search(&seg, &tok, "alpha beta", &[], &params, 10, None).unwrap();
    let doubled = search(&seg, &tok, "alpha beta beta", &[], &params, 10, None).unwrap();
    // With qtf("beta") = 2 the beta contribution doubles, so doc 1's score
    // grows relative to its single-qtf score by exactly its beta component.
    let s1_single = score_of(&single, 1);
    let s1_doubled = score_of(&doubled, 1);
    assert!((s1_doubled - 2.0 * s1_single).abs() < 1e-5);
}

#[test]
fn top_k_and_filter() {
    let seg = build_segment(
        &["body"],
        &[
            (0, vec![("body", "rust here")]),
            (1, vec![("body", "rust there")]),
            (2, vec![("body", "rust everywhere")]),
        ],
    );
    let tok = Tokenizer::default();
    let params = Bm25Params::default();

    let all = search(&seg, &tok, "rust", &[], &params, 10, None).unwrap();
    assert_eq!(all.len(), 3);
    let two = search(&seg, &tok, "rust", &[], &params, 2, None).unwrap();
    assert_eq!(two.len(), 2);

    let pred: &dyn Fn(u32) -> bool = &|d| d != 0;
    let filtered = search(&seg, &tok, "rust", &[], &params, 10, Some(pred)).unwrap();
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|r| r.doc != 0));
}

#[test]
fn empty_and_unmatched_queries() {
    let seg = build_segment(&["body"], &[(0, vec![("body", "hello world")])]);
    let tok = Tokenizer::default();
    let params = Bm25Params::default();
    assert!(search(&seg, &tok, "", &[], &params, 10, None)
        .unwrap()
        .is_empty());
    assert!(search(&seg, &tok, "!!! ---", &[], &params, 10, None)
        .unwrap()
        .is_empty());
    assert!(search(&seg, &tok, "zzzmissing", &[], &params, 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn serialization_roundtrip_preserves_scores() {
    let seg = build_segment(
        &["title", "body"],
        &[
            (0, vec![("title", "rust guide"), ("body", "rust database storage")]),
            (1, vec![("body", "rust search engine")]),
            (3, vec![("body", "database engine engine fast")]),
        ],
    );
    let bytes = seg.to_bytes();
    let seg2 = InvertedSegment::from_bytes(&bytes).unwrap();

    let tok = Tokenizer::default();
    let params = Bm25Params::default();
    for query in ["rust", "engine database", "rust guide engine"] {
        let a = search(&seg, &tok, query, &[], &params, 10, None).unwrap();
        let b = search(&seg2, &tok, query, &[], &params, 10, None).unwrap();
        assert_eq!(a.len(), b.len(), "query '{query}'");
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.doc, y.doc);
            assert!((x.score - y.score).abs() < 1e-6);
        }
    }
}

#[test]
fn corrupted_segments_error_instead_of_panicking() {
    let seg = build_segment(&["body"], &[(0, vec![("body", "hello world")])]);

    // Bad magic.
    let mut bytes = seg.to_bytes();
    bytes[0] = b'X';
    assert!(matches!(
        InvertedSegment::from_bytes(&bytes),
        Err(TextError::BadMagic)
    ));

    // Unsupported version.
    let mut bytes = seg.to_bytes();
    bytes[4] = 99;
    bytes[5] = 0;
    bytes[6] = 0;
    bytes[7] = 0;
    assert!(matches!(
        InvertedSegment::from_bytes(&bytes),
        Err(TextError::UnsupportedVersion(99))
    ));

    // Truncation at every prefix length must error, never panic.
    let bytes = seg.to_bytes();
    for cut in 0..bytes.len() {
        assert!(
            InvertedSegment::from_bytes(&bytes[..cut]).is_err(),
            "truncation at {cut} bytes should be an error"
        );
    }
}
