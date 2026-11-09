//! BM25 ranking over [`InvertedSegment`]s with per-field boosting.
//!
//! Scoring uses the widely deployed non-negative IDF variant:
//!
//! ```text
//! idf(t, f)        = ln(1 + (N_f - df + 0.5) / (df + 0.5))
//! tfnorm(t, d, f)  = tf * (k1 + 1) / (tf + k1 * (1 - b + b * dl / avgdl))
//! score(q, d)      = Σ_t Σ_f  qtf(t) * boost(f) * idf(t, f) * tfnorm(t, d, f)
//! ```
//!
//! where `N_f` is the number of documents containing field `f`, `df` the
//! field-local document frequency of term `t`, `dl` the token length of the
//! document's field, and `avgdl` the average field length. Multi-field
//! scoring is a boost-weighted **sum of per-field BM25 scores** (simple and
//! predictable), not BM25F term-frequency blending; this is documented as a
//! deliberate v1 tradeoff.
//!
//! Score accumulation iterates query terms, fields, and postings in a fixed
//! deterministic order, so identical inputs always produce identical scores
//! and (with the doc-id tiebreak) identical rankings.

use std::collections::{BTreeMap, HashMap};

use super::inverted::{InvertedSegment, TextError};
use super::tokenizer::Tokenizer;

/// BM25 free parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Params {
    /// Term-frequency saturation. Typical range 1.2–2.0.
    pub k1: f32,
    /// Length normalization strength in `[0, 1]`.
    pub b: f32,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75 }
    }
}

/// A scored document (local segment ordinal + BM25 score).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredDoc {
    /// Local doc ordinal within the segment.
    pub doc: u32,
    /// BM25 score (higher is better).
    pub score: f32,
}

/// Non-negative IDF: `ln(1 + (n - df + 0.5) / (df + 0.5))`.
pub fn idf(field_doc_count: u32, doc_freq: u32) -> f32 {
    let n = field_doc_count as f32;
    let df = doc_freq as f32;
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// The term-frequency normalization component (without IDF).
pub fn tf_norm(tf: u32, doc_len: u32, avg_len: f32, params: &Bm25Params) -> f32 {
    let tf = tf as f32;
    let avg = if avg_len > 0.0 { avg_len } else { 1.0 };
    let denom = tf + params.k1 * (1.0 - params.b + params.b * (doc_len as f32) / avg);
    tf * (params.k1 + 1.0) / denom
}

/// Execute a BM25 query against `segment`.
///
/// * `boosts` — per-field `(name, weight)` multipliers. An empty slice means
///   "all fields with weight 1.0". Unknown field names are an error.
/// * `filter` — optional doc-ordinal predicate (e.g. membership in a filter
///   doc-set); documents failing the predicate are never scored.
/// * `top_k` — maximum number of results, ordered by score descending with
///   ascending doc-id tiebreak. Pass `usize::MAX` for all matches.
pub fn search(
    segment: &InvertedSegment,
    tokenizer: &Tokenizer,
    query: &str,
    boosts: &[(String, f32)],
    params: &Bm25Params,
    top_k: usize,
    filter: Option<&dyn Fn(u32) -> bool>,
) -> Result<Vec<ScoredDoc>, TextError> {
    let field_boosts: Vec<(u16, f32)> = if boosts.is_empty() {
        (0..segment.num_fields() as u16).map(|f| (f, 1.0)).collect()
    } else {
        let mut v = Vec::with_capacity(boosts.len());
        for (name, boost) in boosts {
            let id = segment
                .field_id(name)
                .ok_or_else(|| TextError::UnknownField(name.clone()))?;
            v.push((id, *boost));
        }
        v
    };

    // Query term frequencies in deterministic (sorted) iteration order.
    let mut qterms: BTreeMap<String, u32> = BTreeMap::new();
    for tok in tokenizer.tokenize(query) {
        *qterms.entry(tok.text).or_insert(0) += 1;
    }
    if qterms.is_empty() {
        return Ok(Vec::new());
    }

    let mut acc: HashMap<u32, f32> = HashMap::new();
    for (term, qtf) in &qterms {
        for &(field, boost) in &field_boosts {
            if boost == 0.0 {
                continue;
            }
            let Some(stats) = segment.field_stats(field) else {
                continue;
            };
            if stats.doc_count == 0 {
                continue;
            }
            let df = segment.doc_freq(term, field);
            if df == 0 {
                continue;
            }
            let term_idf = idf(stats.doc_count, df);
            let avg_len = segment.avg_field_len(field);
            let Some(postings) = segment.postings(term, field) else {
                continue;
            };
            for (doc, tf) in postings {
                if let Some(f) = filter {
                    if !f(doc) {
                        continue;
                    }
                }
                let dl = segment.doc_len(field, doc);
                let contribution =
                    (*qtf as f32) * boost * term_idf * tf_norm(tf, dl, avg_len, params);
                *acc.entry(doc).or_insert(0.0) += contribution;
            }
        }
    }

    let mut results: Vec<ScoredDoc> = acc
        .into_iter()
        .map(|(doc, score)| ScoredDoc { doc, score })
        .collect();
    results.sort_unstable_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.doc.cmp(&b.doc))
    });
    if top_k < results.len() {
        results.truncate(top_k);
    }
    Ok(results)
}
