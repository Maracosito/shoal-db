//! Baseline query executor: exact dense kNN, sparse dot-product, BM25
//! full-text scoring, metadata filters, and hybrid fusion (RRF / weighted).
//!
//! This executor scans the materialized namespace state. It is the exact
//! reference path; the ANN/inverted-index accelerated paths plug in above it
//! and must produce results consistent with this implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::filter::Filter;
use crate::types::{AttributeValue, DistanceMetric, Document, SparseVector};

use super::state::NamespaceState;
use super::{Engine, EngineError, NsPath, Result};

pub const MAX_TOP_K: usize = 1_000;

fn default_top_k() -> usize {
    10
}
fn default_true() -> bool {
    true
}
fn default_rrf_k() -> f32 {
    60.0
}
fn default_weight() -> f32 {
    1.0
}

#[derive(Debug, Deserialize)]
pub struct TextQuery {
    pub query: String,
    /// Optional per-field boosts. When absent, all text fields are searched
    /// with weight 1.0.
    #[serde(default)]
    pub fields: Option<BTreeMap<String, f32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionMethod {
    Rrf,
    Weighted,
}

#[derive(Debug, Deserialize)]
pub struct FusionSpec {
    #[serde(default = "default_fusion_method")]
    pub method: FusionMethod,
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,
    #[serde(default = "default_weight")]
    pub vector_weight: f32,
    #[serde(default = "default_weight")]
    pub text_weight: f32,
    #[serde(default = "default_weight")]
    pub sparse_weight: f32,
}

fn default_fusion_method() -> FusionMethod {
    FusionMethod::Rrf
}

impl Default for FusionSpec {
    fn default() -> Self {
        FusionSpec {
            method: FusionMethod::Rrf,
            rrf_k: default_rrf_k(),
            vector_weight: 1.0,
            text_weight: 1.0,
            sparse_weight: 1.0,
        }
    }
}

impl FusionSpec {
    fn weight_for(&self, name: &str) -> f32 {
        match name {
            "vector" => self.vector_weight,
            "text" => self.text_weight,
            "sparse" => self.sparse_weight,
            _ => 1.0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    #[serde(default)]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default)]
    pub text: Option<TextQuery>,
    #[serde(default)]
    pub filter: Option<Filter>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Per-query metric override; defaults to the namespace metric.
    #[serde(default)]
    pub distance_metric: Option<DistanceMetric>,
    #[serde(default)]
    pub fusion: Option<FusionSpec>,
    /// Attribute projection. None returns all attributes; Some([]) none.
    #[serde(default)]
    pub include_attributes: Option<Vec<String>>,
    #[serde(default)]
    pub include_vector: bool,
    #[serde(default = "default_true")]
    pub include_text: bool,
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub id: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<BTreeMap<String, String>>,
    pub attrs: BTreeMap<String, AttributeValue>,
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub results: Vec<QueryResult>,
    /// Number of documents that passed the filter and were scored.
    pub candidates: usize,
    /// Execution path chosen: `exact_knn`, `bm25`, `sparse_dot`,
    /// `filter_scan`, `hybrid_rrf`, `hybrid_weighted`.
    pub plan: String,
    pub took_ms: u64,
}

impl Engine {
    pub async fn query(&self, path: &NsPath, req: &QueryRequest) -> Result<QueryResponse> {
        let started = Instant::now();
        let manifest = self.load_manifest(path).await?;
        if let (Some(q), Some(dim)) = (&req.vector, manifest.vector_dim) {
            if q.len() != dim as usize {
                return Err(EngineError::Invalid(format!(
                    "query vector dimension {} does not match namespace dimension {}",
                    q.len(),
                    dim
                )));
            }
        }
        let state = self.namespace_state(path).await?;
        let mut response = execute(&state, manifest.distance_metric.clone(), req)?;
        response.took_ms = started.elapsed().as_millis() as u64;
        Ok(response)
    }

    /// Execute multiple queries against one consistent namespace snapshot.
    pub async fn query_many(
        &self,
        path: &NsPath,
        reqs: &[QueryRequest],
    ) -> Result<Vec<QueryResponse>> {
        if reqs.is_empty() {
            return Err(EngineError::Invalid("queries must not be empty".into()));
        }
        if reqs.len() > 32 {
            return Err(EngineError::Invalid(
                "at most 32 queries per multi-query request".into(),
            ));
        }
        let started = Instant::now();
        let manifest = self.load_manifest(path).await?;
        let state = self.namespace_state(path).await?;
        let mut out = Vec::with_capacity(reqs.len());
        for req in reqs {
            if let (Some(q), Some(dim)) = (&req.vector, manifest.vector_dim) {
                if q.len() != dim as usize {
                    return Err(EngineError::Invalid(format!(
                        "query vector dimension {} does not match namespace dimension {}",
                        q.len(),
                        dim
                    )));
                }
            }
            let mut r = execute(&state, manifest.distance_metric.clone(), req)?;
            r.took_ms = started.elapsed().as_millis() as u64;
            out.push(r);
        }
        Ok(out)
    }
}

/// Pure query execution against a materialized state.
pub fn execute(
    state: &NamespaceState,
    default_metric: DistanceMetric,
    req: &QueryRequest,
) -> Result<QueryResponse> {
    let top_k = req.top_k.clamp(1, MAX_TOP_K);

    // 1. Filter phase: candidate document indices.
    let candidates: Vec<usize> = state
        .docs
        .iter()
        .enumerate()
        .filter(|(_, d)| req.filter.as_ref().map_or(true, |f| f.matches(d)))
        .map(|(i, _)| i)
        .collect();
    let candidate_count = candidates.len();

    // 2. Scoring phase: one ranking per query modality.
    let mut rankings: Vec<(&'static str, Vec<(usize, f32)>)> = Vec::new();

    if let Some(q) = &req.vector {
        let metric = req
            .distance_metric
            .clone()
            .unwrap_or_else(|| default_metric.clone());
        let mut scored: Vec<(usize, f32)> = candidates
            .iter()
            .copied()
            .filter_map(|i| {
                state.docs[i]
                    .vector
                    .as_ref()
                    .filter(|v| v.len() == q.len())
                    .map(|v| (i, vector_score(&metric, q, v)))
            })
            .collect();
        sort_ranking(&mut scored, state);
        rankings.push(("vector", scored));
    }

    if let Some(sq) = &req.sparse_vector {
        let qmap: HashMap<u32, f32> = sq
            .indices
            .iter()
            .copied()
            .zip(sq.values.iter().copied())
            .collect();
        let mut scored: Vec<(usize, f32)> = candidates
            .iter()
            .copied()
            .filter_map(|i| {
                state.docs[i]
                    .sparse_vector
                    .as_ref()
                    .map(|sv| (i, sparse_dot(&qmap, sv)))
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();
        sort_ranking(&mut scored, state);
        rankings.push(("sparse", scored));
    }

    if let Some(tq) = &req.text {
        let scored = bm25_ranking(state, &candidates, tq);
        rankings.push(("text", scored));
    }

    // 3. Fusion / selection phase.
    let (plan, mut fused): (String, Vec<(usize, f32)>) = match rankings.len() {
        0 => {
            let mut listing: Vec<usize> = candidates;
            listing.sort_by(|&a, &b| state.docs[a].id.cmp(&state.docs[b].id));
            (
                "filter_scan".to_string(),
                listing.into_iter().map(|i| (i, 0.0)).collect(),
            )
        }
        1 => {
            let (name, scored) = rankings.pop().expect("len checked");
            let plan = match name {
                "vector" => "exact_knn",
                "sparse" => "sparse_dot",
                "text" => "bm25",
                _ => name,
            };
            (plan.to_string(), scored)
        }
        _ => {
            let spec = req.fusion.as_ref().cloned_or_default();
            match spec.method {
                FusionMethod::Rrf => {
                    let lists: Vec<&Vec<(usize, f32)>> =
                        rankings.iter().map(|(_, r)| r).collect();
                    ("hybrid_rrf".to_string(), rrf_fuse(&lists, spec.rrf_k))
                }
                FusionMethod::Weighted => (
                    "hybrid_weighted".to_string(),
                    weighted_fuse(&rankings, &spec),
                ),
            }
        }
    };

    fused.truncate(top_k);

    // 4. Projection phase.
    let results = fused
        .into_iter()
        .map(|(i, score)| {
            let doc = &state.docs[i];
            let attrs = project_attrs(&doc.attributes, req.include_attributes.as_deref());
            QueryResult {
                id: doc.id.clone(),
                score,
                vector: if req.include_vector {
                    doc.vector.clone()
                } else {
                    None
                },
                text: if req.include_text {
                    Some(doc.text.clone())
                } else {
                    None
                },
                attrs,
            }
        })
        .collect();

    Ok(QueryResponse {
        results,
        candidates: candidate_count,
        plan,
        took_ms: 0,
    })
}

// Small helper so the FusionSpec default doesn't require Clone on the request.
trait ClonedOrDefault {
    fn cloned_or_default(&self) -> FusionSpec;
}

impl ClonedOrDefault for Option<&FusionSpec> {
    fn cloned_or_default(&self) -> FusionSpec {
        match self {
            Some(s) => FusionSpec {
                method: s.method,
                rrf_k: s.rrf_k,
                vector_weight: s.vector_weight,
                text_weight: s.text_weight,
                sparse_weight: s.sparse_weight,
            },
            None => FusionSpec::default(),
        }
    }
}

fn project_attrs(attrs: &BTreeMap<String, AttributeValue>, include: Option<&[String]>) -> BTreeMap<String, AttributeValue> {
    match include {
        None => attrs.clone(),
        Some(keys) => {
            let mut out = BTreeMap::new();
            for k in keys {
                if let Some(v) = attrs.get(k) {
                    out.insert(k.clone(), v.clone());
                }
            }
            out
        }
    }
}

fn sort_ranking(scored: &mut [(usize, f32)], state: &NamespaceState) {
    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| state.docs[a.0].id.cmp(&state.docs[b.0].id))
    });
}

// ---------------------------------------------------------------------------
// Vector math
// ---------------------------------------------------------------------------

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let denom = norm(a) * norm(b);
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot(a, b) / denom
    }
}

pub fn euclidean(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

/// Higher is better for all metrics; Euclidean distance is negated.
pub fn vector_score(metric: &DistanceMetric, q: &[f32], v: &[f32]) -> f32 {
    match metric {
        DistanceMetric::Cosine => cosine(q, v),
        DistanceMetric::Dot => dot(q, v),
        DistanceMetric::Euclidean => -euclidean(q, v),
    }
}

pub fn sparse_dot(query: &HashMap<u32, f32>, doc: &SparseVector) -> f32 {
    doc.indices
        .iter()
        .zip(doc.values.iter())
        .filter_map(|(i, v)| query.get(i).map(|q| q * v))
        .sum()
}

// ---------------------------------------------------------------------------
// BM25
// ---------------------------------------------------------------------------

const BM25_K1: f32 = 1.2;
const BM25_B: f32 = 0.75;

pub fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

struct FieldStats {
    /// term index -> document frequency in this field (corpus-wide).
    df: HashMap<usize, u32>,
    total_len: u64,
    docs_with_field: u32,
    /// candidate doc index -> (field length, term index -> term frequency).
    cand_tf: HashMap<usize, (u32, HashMap<usize, u32>)>,
}

fn bm25_ranking(state: &NamespaceState, candidates: &[usize], tq: &TextQuery) -> Vec<(usize, f32)> {
    let mut terms = tokenize(&tq.query);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Vec::new();
    }
    let term_idx: HashMap<&str, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_str(), i))
        .collect();

    let fields: Vec<(String, f32)> = match &tq.fields {
        Some(m) => m.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        None => {
            let mut set = BTreeSet::new();
            for d in &state.docs {
                for k in d.text.keys() {
                    set.insert(k.clone());
                }
            }
            set.into_iter().map(|k| (k, 1.0)).collect()
        }
    };
    if fields.is_empty() {
        return Vec::new();
    }

    let cand_set: HashSet<usize> = candidates.iter().copied().collect();
    let n_docs = state.docs.len() as f32;

    let mut stats: Vec<FieldStats> = fields
        .iter()
        .map(|_| FieldStats {
            df: HashMap::new(),
            total_len: 0,
            docs_with_field: 0,
            cand_tf: HashMap::new(),
        })
        .collect();

    // Single pass over the corpus: collect df, field lengths, and candidate tf.
    for (di, doc) in state.docs.iter().enumerate() {
        for (fi, (fname, _)) in fields.iter().enumerate() {
            let Some(text) = doc.text.get(fname) else {
                continue;
            };
            let tokens = tokenize(text);
            let st = &mut stats[fi];
            st.total_len += tokens.len() as u64;
            st.docs_with_field += 1;
            let mut tf: HashMap<usize, u32> = HashMap::new();
            for tok in &tokens {
                if let Some(&ti) = term_idx.get(tok.as_str()) {
                    *tf.entry(ti).or_insert(0) += 1;
                }
            }
            for &ti in tf.keys() {
                *st.df.entry(ti).or_insert(0) += 1;
            }
            if cand_set.contains(&di) && !tf.is_empty() {
                st.cand_tf.insert(di, (tokens.len() as u32, tf));
            }
        }
    }

    let mut scores: HashMap<usize, f32> = HashMap::new();
    for (fi, (_, boost)) in fields.iter().enumerate() {
        let st = &stats[fi];
        if st.docs_with_field == 0 {
            continue;
        }
        let avg_len = st.total_len as f32 / st.docs_with_field as f32;
        for (&di, (len, tf)) in &st.cand_tf {
            let mut s = 0.0f32;
            for (&ti, &freq) in tf {
                let df = *st.df.get(&ti).unwrap_or(&0) as f32;
                let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                let f = freq as f32;
                let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * (*len as f32) / avg_len.max(1e-6));
                s += idf * (f * (BM25_K1 + 1.0)) / denom;
            }
            *scores.entry(di).or_insert(0.0) += boost * s;
        }
    }

    let mut ranked: Vec<(usize, f32)> = scores.into_iter().filter(|(_, s)| *s > 0.0).collect();
    ranked.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| state.docs[a.0].id.cmp(&state.docs[b.0].id))
    });
    ranked
}

// ---------------------------------------------------------------------------
// Fusion
// ---------------------------------------------------------------------------

/// Reciprocal Rank Fusion: score(d) = sum over rankings of 1 / (k + rank).
pub fn rrf_fuse(rankings: &[&Vec<(usize, f32)>], k: f32) -> Vec<(usize, f32)> {
    let mut acc: HashMap<usize, f32> = HashMap::new();
    for ranking in rankings {
        for (rank, (i, _)) in ranking.iter().enumerate() {
            *acc.entry(*i).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut out: Vec<(usize, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Weighted score fusion with per-ranking min-max normalization.
pub fn weighted_fuse(
    rankings: &[(&'static str, Vec<(usize, f32)>)],
    spec: &FusionSpec,
) -> Vec<(usize, f32)> {
    let mut acc: HashMap<usize, f32> = HashMap::new();
    for (name, ranking) in rankings {
        let weight = spec.weight_for(name);
        if ranking.is_empty() || weight == 0.0 {
            continue;
        }
        let max = ranking.iter().map(|x| x.1).fold(f32::NEG_INFINITY, f32::max);
        let min = ranking.iter().map(|x| x.1).fold(f32::INFINITY, f32::min);
        let range = max - min;
        for (i, s) in ranking {
            let normalized = if range > f32::EPSILON {
                (s - min) / range
            } else {
                1.0
            };
            *acc.entry(*i).or_insert(0.0) += weight * normalized;
        }
    }
    let mut out: Vec<(usize, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_basic() {
        assert_eq!(
            tokenize("Hello, World! foo_bar 42"),
            vec!["hello", "world", "foo", "bar", "42"]
        );
        assert!(tokenize("--- !!! ").is_empty());
    }

    #[test]
    fn vector_math() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [1.0f32, 0.0, 0.0];
        let c = [0.0f32, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &c).abs() < 1e-6);
        assert!((dot(&a, &b) - 1.0).abs() < 1e-6);
        assert!((euclidean(&a, &c) - 2f32.sqrt()).abs() < 1e-6);
        // Zero vector must not produce NaN.
        let z = [0.0f32, 0.0, 0.0];
        assert_eq!(cosine(&a, &z), 0.0);
    }

    #[test]
    fn rrf_prefers_items_in_both_lists() {
        let r1 = vec![(0usize, 0.9f32), (1, 0.8), (2, 0.7)];
        let r2 = vec![(1usize, 5.0f32), (3, 4.0)];
        let fused = rrf_fuse(&[&r1, &r2], 60.0);
        // Doc 1 appears in both rankings, so it should win.
        assert_eq!(fused[0].0, 1);
    }

    #[test]
    fn weighted_fusion_respects_weights() {
        let rankings = vec![
            ("vector", vec![(0usize, 1.0f32), (1, 0.0)]),
            ("text", vec![(1usize, 1.0f32), (0, 0.0)]),
        ];
        let mut spec = FusionSpec::default();
        spec.method = FusionMethod::Weighted;
        spec.vector_weight = 10.0;
        spec.text_weight = 1.0;
        let fused = weighted_fuse(&rankings, &spec);
        assert_eq!(fused[0].0, 0); // vector winner dominates
    }

    #[test]
    fn weighted_fusion_constant_scores_normalize_to_one() {
        let rankings = vec![("vector", vec![(0usize, 0.5f32), (1, 0.5)])];
        let spec = FusionSpec::default();
        let fused = weighted_fuse(&rankings, &spec);
        assert_eq!(fused.len(), 2);
        assert!((fused[0].1 - 1.0).abs() < 1e-6);
    }
}
