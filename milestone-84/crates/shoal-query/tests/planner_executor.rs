//! End-to-end integration tests for the query planner + executor.
//!
//! These tests build a moderately large in-memory namespace fixture
//! (3,000 documents, 24-dim clustered vectors, two text fields, and a
//! handful of metadata attributes) and then drive the *public* query
//! surface: `QueryRequest` -> `Executor::execute` / `execute_multi`.
//!
//! Coverage:
//!   * plan selection at scale (unfiltered vector -> IVF ANN path)
//!   * adaptive downgrade to exact kNN under highly selective filters
//!   * filtered ANN correctness against a brute-force oracle
//!   * BM25 full-text with field boosting
//!   * hybrid fusion (RRF and weighted) ordering guarantees
//!   * multi-query requests with heterogeneous sub-queries
//!   * attribute projection and top_k semantics
//!   * filter-only execution path
//!   * recall monotonicity in nprobe
//!
//! The fixture is fully deterministic: it uses an inline SplitMix64 PRNG
//! so the test has no dev-dependencies beyond the crate itself.

use std::collections::{BTreeMap, BTreeSet};

use shoal_query::filter::Filter;
use shoal_query::plan::executor::{Executor, MemoryNamespace};
use shoal_query::plan::planner::PlanPath;
use shoal_query::plan::request::{Fusion, QueryRequest};
use shoal_query::types::{AttrValue, Document, Metric};
use shoal_query::vector::ivf::IvfBuildParams;
use shoal_query::vector::math::cosine_similarity;

// ---------------------------------------------------------------------------
// Deterministic PRNG (SplitMix64) — no external dependencies.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f32 in [0, 1).
    fn next_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }

    /// Uniform f32 in [-1, 1).
    fn next_signed(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }

    fn gen_index(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64)) as usize
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

const DIM: usize = 24;
const N_DOCS: usize = 3_000;
const N_CLUSTERS: usize = 16;
const CATEGORIES: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
const TAGS: [&str; 4] = ["red", "green", "blue", "gold"];

/// Per-cluster topic vocabulary used to give BM25 something to bite on.
const TOPIC_WORDS: [&str; 16] = [
    "harbor", "lantern", "granite", "meadow", "compass", "ember", "willow", "orchard",
    "quartz", "saffron", "thicket", "vellum", "anchor", "bramble", "cinder", "drift",
];

const FILLER_WORDS: [&str; 12] = [
    "the", "quick", "system", "stores", "records", "into", "durable", "segments", "across",
    "object", "storage", "tiers",
];

/// The unique "needle" term: appears in exactly one document's title.
const NEEDLE_TERM: &str = "zyzzyva";
const NEEDLE_DOC: usize = 42;
/// A term placed in the *title* of one doc and the *body* of another, to
/// exercise field boosting.
const BOOST_TERM: &str = "kelpwright";
const BOOST_TITLE_DOC: usize = 100;
const BOOST_BODY_DOC: usize = 101;

struct Fixture {
    ns: MemoryNamespace,
    /// Raw vectors retained for brute-force oracles, indexed by doc ordinal.
    vectors: Vec<Vec<f32>>,
    /// Cluster centers, used to synthesize realistic queries.
    centers: Vec<Vec<f32>>,
    /// (category, score, tags) per ordinal, for oracle-side filter checks.
    attrs: Vec<(String, i64, Vec<String>)>,
}

fn doc_id(i: usize) -> String {
    format!("doc-{i:05}")
}

fn build_fixture() -> Fixture {
    let mut rng = Rng::new(0xC0FF_EE5E_ED00_0001);

    // Cluster centers in [-1, 1]^DIM.
    let mut centers = Vec::with_capacity(N_CLUSTERS);
    for _ in 0..N_CLUSTERS {
        let c: Vec<f32> = (0..DIM).map(|_| rng.next_signed()).collect();
        centers.push(c);
    }

    let mut ns = MemoryNamespace::new(Metric::Cosine);
    let mut vectors = Vec::with_capacity(N_DOCS);
    let mut attrs = Vec::with_capacity(N_DOCS);

    for i in 0..N_DOCS {
        let cluster = i % N_CLUSTERS;
        let center = &centers[cluster];

        // Vector: cluster center plus small isotropic noise.
        let vec: Vec<f32> = (0..DIM)
            .map(|d| center[d] + rng.next_signed() * 0.15)
            .collect();

        // Attributes.
        let category = CATEGORIES[cluster % CATEGORIES.len()].to_string();
        let score: i64 = (i % 100) as i64;
        let mut tags: Vec<String> = Vec::new();
        for t in TAGS.iter() {
            if rng.next_f32() < 0.3 {
                tags.push((*t).to_string());
            }
        }
        if tags.is_empty() {
            tags.push("red".to_string());
        }

        // Text: title leads with the cluster topic word; body is filler with
        // occasional topic repetition.
        let topic = TOPIC_WORDS[cluster];
        let mut title = format!("{topic} report number {i}");
        let mut body_words: Vec<&str> = Vec::with_capacity(24);
        for _ in 0..20 {
            body_words.push(FILLER_WORDS[rng.gen_index(FILLER_WORDS.len())]);
        }
        if rng.next_f32() < 0.5 {
            body_words.push(topic);
        }
        let mut body = body_words.join(" ");

        if i == NEEDLE_DOC {
            title = format!("{NEEDLE_TERM} {title}");
        }
        if i == BOOST_TITLE_DOC {
            title = format!("{BOOST_TERM} {title}");
        }
        if i == BOOST_BODY_DOC {
            body = format!("{body} {BOOST_TERM}");
        }

        let doc = Document::new(doc_id(i))
            .with_vector(vec.clone())
            .with_text("title", title)
            .with_text("body", body)
            .with_attr("category", AttrValue::from(category.as_str()))
            .with_attr("score", AttrValue::from(score))
            .with_attr(
                "tags",
                AttrValue::from(tags.iter().map(|s| s.as_str()).collect::<Vec<_>>()),
            )
            .with_attr("cluster", AttrValue::from(cluster as i64));

        ns.insert(doc);
        vectors.push(vec);
        attrs.push((category, score, tags));
    }

    ns.build_indexes(IvfBuildParams {
        nlist: N_CLUSTERS,
        max_iters: 12,
        seed: 7,
        ..IvfBuildParams::default()
    })
    .expect("index build must succeed");

    Fixture {
        ns,
        vectors,
        centers,
        attrs,
    }
}

/// A query vector near a given cluster center, with mild noise.
fn query_near_cluster(rng: &mut Rng, fx: &Fixture, cluster: usize) -> Vec<f32> {
    fx.centers[cluster]
        .iter()
        .map(|c| c + rng.next_signed() * 0.05)
        .collect()
}

/// Brute-force cosine top-k over an optional ordinal predicate. Returns
/// (id, score) sorted by score descending, ties broken by ordinal for
/// determinism.
fn brute_force_topk(
    fx: &Fixture,
    query: &[f32],
    k: usize,
    pred: impl Fn(usize) -> bool,
) -> Vec<(String, f32)> {
    let mut scored: Vec<(usize, f32)> = fx
        .vectors
        .iter()
        .enumerate()
        .filter(|(i, _)| pred(*i))
        .map(|(i, v)| (i, cosine_similarity(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(k)
        .map(|(i, s)| (doc_id(i), s))
        .collect()
}

fn ids_of(hits: &[(String, f32)]) -> BTreeSet<&str> {
    hits.iter().map(|(id, _)| id.as_str()).collect()
}

fn assert_scores_descending(scores: impl Iterator<Item = f32>) {
    let collected: Vec<f32> = scores.collect();
    for w in collected.windows(2) {
        assert!(
            w[0] >= w[1] - 1e-6,
            "scores must be non-increasing: {} then {}",
            w[0],
            w[1]
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Plan selection at scale: unfiltered vector query goes through IVF ANN.
// ---------------------------------------------------------------------------

#[test]
fn unfiltered_vector_query_uses_ivf_and_has_good_recall() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(11);

    let n_queries = 20;
    let k = 10;
    let mut total_overlap = 0usize;

    for q in 0..n_queries {
        let qvec = query_near_cluster(&mut rng, &fx, q % N_CLUSTERS);

        let req = QueryRequest::new()
            .vector(qvec.clone())
            .top_k(k)
            .nprobe(4);
        let res = exec.execute(&req).expect("query must succeed");

        // Plan: with 3,000 docs and a built IVF index, the planner must
        // choose the ANN path, not exact scan.
        assert_eq!(
            res.plan.path,
            PlanPath::IvfAnn,
            "expected IVF ANN plan for unfiltered vector query at scale, got {:?}",
            res.plan.path
        );
        assert!(!res.plan.downgraded_to_exact);

        assert_eq!(res.hits.len(), k);
        assert_scores_descending(res.hits.iter().map(|h| h.score));

        let oracle = brute_force_topk(&fx, &qvec, k, |_| true);
        let oracle_ids = ids_of(&oracle);
        let got_ids: BTreeSet<&str> = res.hits.iter().map(|h| h.id.as_str()).collect();
        total_overlap += got_ids.intersection(&oracle_ids).count();
    }

    let recall = total_overlap as f64 / (n_queries * k) as f64;
    assert!(
        recall >= 0.85,
        "mean recall@{k} with nprobe=4 over clustered data should be >= 0.85, got {recall:.3}"
    );
}

// ---------------------------------------------------------------------------
// 2. Adaptive exact downgrade under a highly selective filter.
// ---------------------------------------------------------------------------

#[test]
fn selective_filter_downgrades_to_exact_and_matches_oracle() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(23);

    // category == "alpha" AND score > 90 selects roughly 3000/4 * 9/100 ≈ 67
    // documents — far below any sane ANN threshold, so the planner must
    // downgrade to exact scoring over the filtered candidate set.
    let filter = Filter::and(vec![
        Filter::eq("category", "alpha"),
        Filter::gt("score", 90i64),
    ]);

    let qvec = query_near_cluster(&mut rng, &fx, 0); // cluster 0 -> category alpha
    let k = 10;

    let req = QueryRequest::new()
        .vector(qvec.clone())
        .filter(filter)
        .top_k(k);
    let res = exec.execute(&req).expect("query must succeed");

    assert!(
        res.plan.downgraded_to_exact || res.plan.path == PlanPath::ExactKnn,
        "selective filter must trigger exact execution, plan was {:?}",
        res.plan
    );

    // Every hit must satisfy the filter (verified against fixture-side attrs).
    for hit in &res.hits {
        let ord: usize = hit.id["doc-".len()..].parse().unwrap();
        let (cat, score, _) = &fx.attrs[ord];
        assert_eq!(cat, "alpha", "hit {} violates category filter", hit.id);
        assert!(*score > 90, "hit {} violates score filter", hit.id);
    }

    // Exact execution over a filtered set must match the brute-force oracle
    // exactly (same ids, same order, same scores up to float tolerance).
    let oracle = brute_force_topk(&fx, &qvec, k, |i| {
        let (cat, score, _) = &fx.attrs[i];
        cat == "alpha" && *score > 90
    });
    assert_eq!(res.hits.len(), oracle.len());
    for (hit, (oid, oscore)) in res.hits.iter().zip(oracle.iter()) {
        assert_eq!(&hit.id, oid, "exact filtered results must match oracle order");
        assert!(
            (hit.score - oscore).abs() < 1e-4,
            "score mismatch for {}: {} vs oracle {}",
            hit.id,
            hit.score,
            oscore
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Broad filter keeps ANN path; results respect the filter with good recall.
// ---------------------------------------------------------------------------

#[test]
fn broad_filter_uses_ann_and_respects_filter() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(37);

    // category == "beta" selects ~750 docs (clusters 1, 5, 9, 13) — large
    // enough that the planner should keep the IVF path with filtered
    // probing/post-filtering rather than downgrading to exact.
    let filter = Filter::eq("category", "beta");
    let k = 10;
    let qvec = query_near_cluster(&mut rng, &fx, 1); // cluster 1 -> beta

    let req = QueryRequest::new()
        .vector(qvec.clone())
        .filter(filter)
        .top_k(k)
        .nprobe(6);
    let res = exec.execute(&req).expect("query must succeed");

    assert_eq!(res.plan.path, PlanPath::IvfAnn);

    assert_eq!(res.hits.len(), k);
    for hit in &res.hits {
        let ord: usize = hit.id["doc-".len()..].parse().unwrap();
        assert_eq!(fx.attrs[ord].0, "beta", "hit {} violates filter", hit.id);
    }

    let oracle = brute_force_topk(&fx, &qvec, k, |i| fx.attrs[i].0 == "beta");
    let overlap = ids_of(&oracle)
        .intersection(&res.hits.iter().map(|h| h.id.as_str()).collect())
        .count();
    assert!(
        overlap >= 7,
        "filtered ANN recall@{k} should be >= 0.7 on clustered data, overlap was {overlap}"
    );
}

// ---------------------------------------------------------------------------
// 4. Full-text BM25 with field boosting.
// ---------------------------------------------------------------------------

#[test]
fn full_text_finds_needle_and_respects_field_boosts() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);

    // The needle term appears exactly once, in doc-00042's title.
    let req = QueryRequest::new().text(NEEDLE_TERM).top_k(5);
    let res = exec.execute(&req).expect("query must succeed");
    assert_eq!(res.plan.path, PlanPath::FullText);
    assert!(!res.hits.is_empty(), "needle term must match");
    assert_eq!(res.hits[0].id, doc_id(NEEDLE_DOC));

    // BOOST_TERM is in doc-00100's *title* and doc-00101's *body*. With a
    // strong title boost, the title occurrence must rank first.
    let boosted = QueryRequest::new()
        .text(BOOST_TERM)
        .field_boost("title", 3.0)
        .field_boost("body", 1.0)
        .top_k(5);
    let res = exec.execute(&boosted).expect("query must succeed");
    let ids: Vec<&str> = res.hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&doc_id(BOOST_TITLE_DOC).as_str()));
    assert!(ids.contains(&doc_id(BOOST_BODY_DOC).as_str()));
    assert_eq!(
        res.hits[0].id,
        doc_id(BOOST_TITLE_DOC),
        "title boost must rank title occurrence above body occurrence"
    );

    // Invert the boost: body occurrence should now rank first. (Titles are
    // shorter, so the inversion must be strong enough to dominate length
    // normalization.)
    let inverted = QueryRequest::new()
        .text(BOOST_TERM)
        .field_boost("title", 0.1)
        .field_boost("body", 5.0)
        .top_k(5);
    let res = exec.execute(&inverted).expect("query must succeed");
    assert_eq!(
        res.hits[0].id,
        doc_id(BOOST_BODY_DOC),
        "body boost must rank body occurrence first"
    );

    // BM25 scores must be positive and non-increasing.
    assert_scores_descending(res.hits.iter().map(|h| h.score));
    assert!(res.hits.iter().all(|h| h.score > 0.0));
}

// ---------------------------------------------------------------------------
// 5. Hybrid fusion: RRF and weighted score fusion.
// ---------------------------------------------------------------------------

#[test]
fn hybrid_rrf_promotes_doc_strong_in_both_modalities() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(53);

    // doc-00042 lives in cluster 42 % 16 = 10. Query its cluster center AND
    // its unique needle term: it should win under RRF because it is the only
    // document strong in both ranked lists.
    let cluster = NEEDLE_DOC % N_CLUSTERS;
    let qvec = query_near_cluster(&mut rng, &fx, cluster);

    let req = QueryRequest::new()
        .vector(qvec)
        .text(NEEDLE_TERM)
        .fusion(Fusion::Rrf { k: 60.0 })
        .top_k(10);
    let res = exec.execute(&req).expect("hybrid query must succeed");

    assert_eq!(res.plan.path, PlanPath::Hybrid);
    assert_eq!(
        res.hits[0].id,
        doc_id(NEEDLE_DOC),
        "RRF must rank the doc that is top-ranked in BM25 and present in vector results first"
    );
    assert_scores_descending(res.hits.iter().map(|h| h.score));
}

#[test]
fn weighted_fusion_extremes_reduce_to_single_modality_ordering() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(59);

    let cluster = 3;
    let qvec = query_near_cluster(&mut rng, &fx, cluster);
    let topic = TOPIC_WORDS[5]; // deliberately a *different* cluster's topic
    let k = 10;

    // Vector-only baseline.
    let vec_only = exec
        .execute(&QueryRequest::new().vector(qvec.clone()).top_k(k).nprobe(8))
        .unwrap();
    // Text-only baseline.
    let text_only = exec
        .execute(&QueryRequest::new().text(topic).top_k(k))
        .unwrap();

    // Weighted fusion with all weight on the vector side must reproduce the
    // vector-only ordering.
    let all_vector = exec
        .execute(
            &QueryRequest::new()
                .vector(qvec.clone())
                .text(topic)
                .fusion(Fusion::Weighted {
                    vector_weight: 1.0,
                    text_weight: 0.0,
                })
                .top_k(k)
                .nprobe(8),
        )
        .unwrap();
    assert_eq!(all_vector.plan.path, PlanPath::Hybrid);
    let vec_ids: Vec<&str> = vec_only.hits.iter().map(|h| h.id.as_str()).collect();
    let fused_ids: Vec<&str> = all_vector.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        fused_ids, vec_ids,
        "weighted fusion with text_weight=0 must equal vector-only ordering"
    );

    // All weight on text must reproduce the text-only ordering.
    let all_text = exec
        .execute(
            &QueryRequest::new()
                .vector(qvec)
                .text(topic)
                .fusion(Fusion::Weighted {
                    vector_weight: 0.0,
                    text_weight: 1.0,
                })
                .top_k(k),
        )
        .unwrap();
    let text_ids: Vec<&str> = text_only.hits.iter().map(|h| h.id.as_str()).collect();
    let fused_ids: Vec<&str> = all_text.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        fused_ids, text_ids,
        "weighted fusion with vector_weight=0 must equal text-only ordering"
    );
}

// ---------------------------------------------------------------------------
// 6. Multi-query requests with heterogeneous sub-queries.
// ---------------------------------------------------------------------------

#[test]
fn multi_query_executes_each_sub_query_independently() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(71);

    let qvec = query_near_cluster(&mut rng, &fx, 2);

    let requests = vec![
        // Pure vector.
        QueryRequest::new().vector(qvec.clone()).top_k(5).nprobe(4),
        // Pure text.
        QueryRequest::new().text(NEEDLE_TERM).top_k(3),
        // Hybrid with a filter.
        QueryRequest::new()
            .vector(qvec.clone())
            .text(TOPIC_WORDS[2])
            .filter(Filter::eq("category", "gamma")) // cluster 2 -> gamma
            .fusion(Fusion::Rrf { k: 60.0 })
            .top_k(5),
    ];

    let responses = exec
        .execute_multi(&requests)
        .expect("multi-query must succeed");
    assert_eq!(responses.len(), 3);

    // Sub-query 0: vector path, 5 hits, matches a standalone execution.
    assert_eq!(responses[0].plan.path, PlanPath::IvfAnn);
    assert_eq!(responses[0].hits.len(), 5);
    let standalone = exec
        .execute(&QueryRequest::new().vector(qvec).top_k(5).nprobe(4))
        .unwrap();
    let multi_ids: Vec<&str> = responses[0].hits.iter().map(|h| h.id.as_str()).collect();
    let solo_ids: Vec<&str> = standalone.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        multi_ids, solo_ids,
        "a sub-query inside a batch must return the same results as standalone"
    );

    // Sub-query 1: text path, needle found.
    assert_eq!(responses[1].plan.path, PlanPath::FullText);
    assert_eq!(responses[1].hits[0].id, doc_id(NEEDLE_DOC));

    // Sub-query 2: hybrid path, all hits satisfy the filter.
    assert_eq!(responses[2].plan.path, PlanPath::Hybrid);
    for hit in &responses[2].hits {
        let ord: usize = hit.id["doc-".len()..].parse().unwrap();
        assert_eq!(fx.attrs[ord].0, "gamma", "hit {} violates filter", hit.id);
    }
}

// ---------------------------------------------------------------------------
// 7. Attribute projection and top_k semantics.
// ---------------------------------------------------------------------------

#[test]
fn projection_returns_only_requested_attributes() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);
    let mut rng = Rng::new(83);

    let qvec = query_near_cluster(&mut rng, &fx, 4);

    // Without projection: no attributes are materialized (ids + scores only).
    let bare = exec
        .execute(&QueryRequest::new().vector(qvec.clone()).top_k(5))
        .unwrap();
    for hit in &bare.hits {
        assert!(
            hit.attributes.is_empty(),
            "hits must carry no attributes unless projection is requested"
        );
    }

    // With projection: exactly the requested keys, with correct values.
    let projected = exec
        .execute(
            &QueryRequest::new()
                .vector(qvec)
                .top_k(5)
                .include_attributes(["category", "score"]),
        )
        .unwrap();
    for hit in &projected.hits {
        let keys: BTreeSet<&str> = hit.attributes.keys().map(|k| k.as_str()).collect();
        let expected: BTreeSet<&str> = ["category", "score"].into_iter().collect();
        assert_eq!(keys, expected, "projection must return exactly the requested keys");

        let ord: usize = hit.id["doc-".len()..].parse().unwrap();
        let (cat, score, _) = &fx.attrs[ord];
        assert_eq!(hit.attributes["category"], AttrValue::from(cat.as_str()));
        assert_eq!(hit.attributes["score"], AttrValue::from(*score));
    }
}

#[test]
fn top_k_is_respected_and_clamped_to_match_count() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);

    // top_k smaller than the corpus.
    let res = exec
        .execute(&QueryRequest::new().text(TOPIC_WORDS[0]).top_k(5))
        .unwrap();
    assert_eq!(res.hits.len(), 5);

    // top_k larger than the match count: needle matches exactly one doc.
    let res = exec
        .execute(&QueryRequest::new().text(NEEDLE_TERM).top_k(50))
        .unwrap();
    assert_eq!(res.hits.len(), 1);
    assert_eq!(res.hits[0].id, doc_id(NEEDLE_DOC));
}

// ---------------------------------------------------------------------------
// 8. Filter-only execution path (no vector, no text).
// ---------------------------------------------------------------------------

#[test]
fn filter_only_query_uses_filter_path_and_matches_predicate() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);

    // tags ContainsAny ["gold"] AND NOT category == "delta", scores in (20, 40].
    let filter = Filter::and(vec![
        Filter::contains_any("tags", vec!["gold"]),
        Filter::not(Filter::eq("category", "delta")),
        Filter::gt("score", 20i64),
        Filter::lte("score", 40i64),
    ]);

    let req = QueryRequest::new().filter(filter).top_k(1_000);
    let res = exec.execute(&req).expect("filter-only query must succeed");
    assert_eq!(res.plan.path, PlanPath::FilterOnly);

    // Oracle count over the fixture-side attribute mirror.
    let oracle_count = fx
        .attrs
        .iter()
        .filter(|(cat, score, tags)| {
            tags.iter().any(|t| t == "gold") && cat != "delta" && *score > 20 && *score <= 40
        })
        .count();
    assert!(oracle_count > 0, "fixture must produce some matches");
    assert_eq!(
        res.hits.len(),
        oracle_count.min(1_000),
        "filter-only result count must match the oracle"
    );

    for hit in &res.hits {
        let ord: usize = hit.id["doc-".len()..].parse().unwrap();
        let (cat, score, tags) = &fx.attrs[ord];
        assert!(tags.iter().any(|t| t == "gold"));
        assert_ne!(cat, "delta");
        assert!(*score > 20 && *score <= 40);
    }

    // `In` on a string attribute composes with the rest of the algebra.
    let in_filter = Filter::is_in("category", vec!["alpha", "beta"]);
    let res = exec
        .execute(&QueryRequest::new().filter(in_filter).top_k(5_000))
        .unwrap();
    let oracle_count = fx
        .attrs
        .iter()
        .filter(|(cat, _, _)| cat == "alpha" || cat == "beta")
        .count();
    assert_eq!(res.hits.len(), oracle_count);
}

// ---------------------------------------------------------------------------
// 9. Planner consistency: increasing nprobe never reduces recall.
// ---------------------------------------------------------------------------

#[test]
fn recall_is_monotonic_in_nprobe() {
    let fx = build_fixture();
    let exec = Executor::new(&fx.ns);

    let k = 10;
    let n_queries = 10;
    let mut recall_at = BTreeMap::new();

    for &nprobe in &[1usize, 4, 16] {
        let mut overlap = 0usize;
        // Same query stream for every nprobe setting.
        let mut qrng = Rng::new(0xDEAD_BEEF_0000_0001);
        for q in 0..n_queries {
            let qvec = query_near_cluster(&mut qrng, &fx, q % N_CLUSTERS);
            let res = exec
                .execute(
                    &QueryRequest::new()
                        .vector(qvec.clone())
                        .top_k(k)
                        .nprobe(nprobe),
                )
                .unwrap();
            let oracle = brute_force_topk(&fx, &qvec, k, |_| true);
            overlap += ids_of(&oracle)
                .intersection(&res.hits.iter().map(|h| h.id.as_str()).collect())
                .count();
        }
        recall_at.insert(nprobe, overlap as f64 / (n_queries * k) as f64);
    }

    assert!(
        recall_at[&4] >= recall_at[&1] - 1e-9,
        "recall must not decrease when nprobe grows: {:?}",
        recall_at
    );
    assert!(
        recall_at[&16] >= recall_at[&4] - 1e-9,
        "recall must not decrease when nprobe grows: {:?}",
        recall_at
    );
    // Probing every list is exhaustive over the index — recall must be ~1.0.
    assert!(
        recall_at[&16] >= 0.99,
        "nprobe == nlist must be exhaustive, got recall {:.3}",
        recall_at[&16]
    );
}
