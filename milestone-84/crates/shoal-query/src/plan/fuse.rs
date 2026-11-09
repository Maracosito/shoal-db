//! Plan-level result fusion across retrieval legs.
//!
//! These functions operate on the executor's per-leg candidate lists (already
//! sorted, higher-is-better) and merge them into a single ranking. Two
//! strategies are provided, mirroring the request-level [`FusionSpec`]
//! (`crate::plan::request::FusionSpec`):
//!
//! * **Reciprocal rank fusion (RRF)** — rank-based, scale-free; robust when
//!   leg scores live on incomparable scales (BM25 vs cosine similarity).
//! * **Weighted score fusion** — combines raw or per-leg min-max-normalized
//!   scores with caller-provided weights.
//!
//! Both are deterministic: ties are broken by ascending document id.

use super::Hit;
use std::collections::HashMap;

/// Sort hits by descending score, breaking ties by ascending doc id.
pub fn sort_hits_desc(hits: &mut [Hit]) {
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
}

/// Reciprocal rank fusion.
///
/// For each leg `i` with weight `w_i`, a document at rank `r` (1-based)
/// contributes `w_i / (k + r)`. Missing weights default to `1.0`; legs with
/// weight `0.0` are skipped entirely.
pub fn rrf_fuse(legs: &[Vec<Hit>], weights: &[f32], k: f32) -> Vec<Hit> {
    let mut acc: HashMap<u64, f32> = HashMap::new();
    for (i, leg) in legs.iter().enumerate() {
        let w = weights.get(i).copied().unwrap_or(1.0);
        if w == 0.0 {
            continue;
        }
        for (rank0, hit) in leg.iter().enumerate() {
            *acc.entry(hit.doc).or_insert(0.0) += w / (k + rank0 as f32 + 1.0);
        }
    }
    let mut out: Vec<Hit> = acc
        .into_iter()
        .map(|(doc, score)| Hit { doc, score })
        .collect();
    sort_hits_desc(&mut out);
    out
}

/// Weighted score fusion.
///
/// When `normalize` is set, each leg's scores are min-max scaled into
/// `[0, 1]` before weighting (a leg whose scores are all identical maps to
/// `1.0`). Documents appearing in multiple legs accumulate contributions.
pub fn weighted_fuse(legs: &[Vec<Hit>], weights: &[f32], normalize: bool) -> Vec<Hit> {
    let mut acc: HashMap<u64, f32> = HashMap::new();
    for (i, leg) in legs.iter().enumerate() {
        let w = weights.get(i).copied().unwrap_or(1.0);
        if w == 0.0 || leg.is_empty() {
            continue;
        }
        let (lo, hi) = if normalize { min_max(leg) } else { (0.0, 0.0) };
        for hit in leg {
            let s = if normalize {
                norm_score(hit.score, lo, hi)
            } else {
                hit.score
            };
            *acc.entry(hit.doc).or_insert(0.0) += w * s;
        }
    }
    let mut out: Vec<Hit> = acc
        .into_iter()
        .map(|(doc, score)| Hit { doc, score })
        .collect();
    sort_hits_desc(&mut out);
    out
}

fn min_max(hits: &[Hit]) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for h in hits {
        if h.score < lo {
            lo = h.score;
        }
        if h.score > hi {
            hi = h.score;
        }
    }
    (lo, hi)
}

fn norm_score(s: f32, lo: f32, hi: f32) -> f32 {
    if hi - lo <= f32::EPSILON {
        1.0
    } else {
        (s - lo) / (hi - lo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(doc: u64, score: f32) -> Hit {
        Hit { doc, score }
    }

    fn docs(hits: &[Hit]) -> Vec<u64> {
        hits.iter().map(|h| h.doc).collect()
    }

    #[test]
    fn rrf_merges_duplicates_and_orders() {
        // leg1: a(1), b(2); leg2: b(1), c(2). With k=60:
        //   a = 1/61; b = 1/62 + 1/61; c = 1/62 → order b, a, c.
        let leg1 = vec![h(1, 0.9), h(2, 0.8)];
        let leg2 = vec![h(2, 12.0), h(3, 4.0)];
        let fused = rrf_fuse(&[leg1, leg2], &[1.0, 1.0], 60.0);
        assert_eq!(docs(&fused), vec![2, 1, 3]);
        let expected_b = 1.0 / 62.0 + 1.0 / 61.0;
        assert!((fused[0].score - expected_b).abs() < 1e-6);
    }

    #[test]
    fn rrf_respects_weights() {
        // Zero weight on leg1 removes its contribution entirely.
        let leg1 = vec![h(1, 0.9), h(2, 0.8)];
        let leg2 = vec![h(2, 12.0), h(3, 4.0)];
        let fused = rrf_fuse(&[leg1, leg2], &[0.0, 1.0], 60.0);
        assert_eq!(docs(&fused), vec![2, 3]);
    }

    #[test]
    fn rrf_missing_weights_default_to_one() {
        let leg1 = vec![h(1, 0.9)];
        let leg2 = vec![h(2, 0.5)];
        let fused = rrf_fuse(&[leg1, leg2], &[], 60.0);
        // both at rank 1 with weight 1 → tie broken by doc id
        assert_eq!(docs(&fused), vec![1, 2]);
        assert!((fused[0].score - fused[1].score).abs() < 1e-9);
    }

    #[test]
    fn weighted_normalizes_per_leg() {
        // leg1 raw scores: a=10, b=0 → normalized a=1, b=0.
        // leg2 raw scores: b=5, c=1 → normalized b=1, c=0.
        // sums: a=1, b=1 (tie → a first by id), c=0.
        let leg1 = vec![h(1, 10.0), h(2, 0.0)];
        let leg2 = vec![h(2, 5.0), h(3, 1.0)];
        let fused = weighted_fuse(&[leg1, leg2], &[1.0, 1.0], true);
        assert_eq!(docs(&fused), vec![1, 2, 3]);
        assert!((fused[0].score - 1.0).abs() < 1e-6);
        assert!((fused[1].score - 1.0).abs() < 1e-6);
        assert!(fused[2].score.abs() < 1e-6);
    }

    #[test]
    fn weighted_without_normalization_uses_raw_scores() {
        let leg1 = vec![h(1, 2.0)];
        let leg2 = vec![h(1, 3.0), h(2, 10.0)];
        let fused = weighted_fuse(&[leg1, leg2], &[1.0, 0.5], false);
        // doc1 = 2.0 + 1.5 = 3.5; doc2 = 5.0 → doc2 first.
        assert_eq!(docs(&fused), vec![2, 1]);
        assert!((fused[0].score - 5.0).abs() < 1e-6);
        assert!((fused[1].score - 3.5).abs() < 1e-6);
    }

    #[test]
    fn weighted_constant_leg_maps_to_one() {
        let leg = vec![h(1, 7.0), h(2, 7.0), h(3, 7.0)];
        let fused = weighted_fuse(&[leg], &[1.0], true);
        assert_eq!(fused.len(), 3);
        for f in &fused {
            assert!((f.score - 1.0).abs() < 1e-6);
        }
        // deterministic tie-break by id
        assert_eq!(docs(&fused), vec![1, 2, 3]);
    }

    #[test]
    fn empty_legs_produce_empty_output() {
        assert!(rrf_fuse(&[], &[], 60.0).is_empty());
        assert!(weighted_fuse(&[vec![], vec![]], &[1.0, 1.0], true).is_empty());
    }
}
