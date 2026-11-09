//! Score fusion for hybrid and multi-query search.
//!
//! Two fusion strategies are provided, both operating on ranked result lists
//! of `(id, score)` pairs where **higher scores are better** (the executor is
//! responsible for converting distance metrics like Euclidean into
//! higher-is-better similarities before fusion):
//!
//! * [`weighted_fusion`] — per-list score normalization followed by a
//!   weighted sum of scores. Best when score magnitudes are meaningful.
//! * [`reciprocal_rank_fusion`] — rank-based RRF
//!   (`Σ weight / (k + rank)`), robust to incomparable score scales; the de
//!   facto standard for combining BM25 with vector similarity.
//!
//! Multi-query requests reuse the same machinery: each sub-query produces one
//! ranked list, and the lists are fused with either strategy.
//!
//! Determinism: final results are ordered by fused score descending, with
//! ascending-id tiebreak (`I: Ord`). If the same id appears multiple times in
//! a single input list, each occurrence contributes — callers should dedupe
//! per-list results first (the executor does).

use std::collections::HashMap;
use std::hash::Hash;

/// Default RRF smoothing constant, the value used in the original RRF paper
/// and most production systems.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Per-list score normalization applied before weighted fusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScoreNorm {
    /// Use raw scores unchanged.
    None,
    /// Min–max normalize each list to `[0, 1]`. A list whose scores are all
    /// equal normalizes to `1.0` for every entry.
    MinMax,
}

/// One ranked input list with its fusion weight.
#[derive(Debug, Clone, Copy)]
pub struct RankedList<'a, I> {
    /// Multiplicative weight for this list's contribution.
    pub weight: f32,
    /// `(id, score)` pairs; higher score is better. Order does not need to be
    /// sorted — ranking is derived internally where needed.
    pub items: &'a [(I, f32)],
}

/// Normalize one list's scores according to `norm`.
pub fn normalize_scores<I: Clone>(items: &[(I, f32)], norm: ScoreNorm) -> Vec<(I, f32)> {
    match norm {
        ScoreNorm::None => items.to_vec(),
        ScoreNorm::MinMax => {
            if items.is_empty() {
                return Vec::new();
            }
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            for (_, s) in items {
                if *s < min {
                    min = *s;
                }
                if *s > max {
                    max = *s;
                }
            }
            let range = max - min;
            items
                .iter()
                .map(|(id, s)| {
                    let n = if range > 0.0 { (s - min) / range } else { 1.0 };
                    (id.clone(), n)
                })
                .collect()
        }
    }
}

/// Weighted score fusion: `fused(id) = Σ_list weight * norm(score)`.
///
/// Returns up to `top_k` results ordered by fused score descending with
/// ascending-id tiebreak.
pub fn weighted_fusion<I: Clone + Eq + Hash + Ord>(
    lists: &[RankedList<'_, I>],
    norm: ScoreNorm,
    top_k: usize,
) -> Vec<(I, f32)> {
    let mut acc: HashMap<I, f32> = HashMap::new();
    for list in lists {
        for (id, score) in normalize_scores(list.items, norm) {
            *acc.entry(id).or_insert(0.0) += list.weight * score;
        }
    }
    finalize(acc, top_k)
}

/// Reciprocal rank fusion: `fused(id) = Σ_list weight / (k + rank)` where
/// `rank` is the 1-based position of `id` in the list ordered by score
/// descending (ties broken by original list position).
///
/// `k` controls how quickly contribution decays with rank; use
/// [`DEFAULT_RRF_K`] unless you have a reason not to. Returns up to `top_k`
/// results ordered by fused score descending with ascending-id tiebreak.
pub fn reciprocal_rank_fusion<I: Clone + Eq + Hash + Ord>(
    lists: &[RankedList<'_, I>],
    k: f32,
    top_k: usize,
) -> Vec<(I, f32)> {
    let mut acc: HashMap<I, f32> = HashMap::new();
    for list in lists {
        // Derive ranks: sort indices by score descending, stable on original
        // position for ties.
        let mut order: Vec<usize> = (0..list.items.len()).collect();
        order.sort_by(|&a, &b| {
            list.items[b]
                .1
                .total_cmp(&list.items[a].1)
                .then_with(|| a.cmp(&b))
        });
        for (rank0, &idx) in order.iter().enumerate() {
            let (id, _) = &list.items[idx];
            *acc.entry(id.clone()).or_insert(0.0) += list.weight / (k + (rank0 as f32 + 1.0));
        }
    }
    finalize(acc, top_k)
}

fn finalize<I: Ord>(acc: HashMap<I, f32>, top_k: usize) -> Vec<(I, f32)> {
    let mut results: Vec<(I, f32)> = acc.into_iter().collect();
    results.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if top_k < results.len() {
        results.truncate(top_k);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(results: &[(String, f32)]) -> Vec<&str> {
        results.iter().map(|(id, _)| id.as_str()).collect()
    }

    fn list(items: &[(&str, f32)]) -> Vec<(String, f32)> {
        items.iter().map(|(s, v)| (s.to_string(), *v)).collect()
    }

    #[test]
    fn rrf_hand_computed() {
        let l1 = list(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        let l2 = list(&[("b", 9.0), ("c", 8.0)]);
        let lists = [
            RankedList { weight: 1.0, items: &l1 },
            RankedList { weight: 1.0, items: &l2 },
        ];
        let fused = reciprocal_rank_fusion(&lists, DEFAULT_RRF_K, 10);
        let expected_a = 1.0 / 61.0;
        let expected_b = 1.0 / 62.0 + 1.0 / 61.0;
        let expected_c = 1.0 / 63.0 + 1.0 / 62.0;
        assert_eq!(ids(&fused), vec!["b", "c", "a"]);
        let by_id: HashMap<_, _> = fused.iter().map(|(i, s)| (i.as_str(), *s)).collect();
        assert!((by_id["a"] - expected_a).abs() < 1e-7);
        assert!((by_id["b"] - expected_b).abs() < 1e-7);
        assert!((by_id["c"] - expected_c).abs() < 1e-7);
    }

    #[test]
    fn rrf_respects_weights() {
        let l1 = list(&[("a", 1.0)]);
        let l2 = list(&[("b", 1.0)]);
        let lists = [
            RankedList { weight: 1.0, items: &l1 },
            RankedList { weight: 3.0, items: &l2 },
        ];
        let fused = reciprocal_rank_fusion(&lists, DEFAULT_RRF_K, 10);
        assert_eq!(ids(&fused), vec!["b", "a"]);
        assert!((fused[0].1 - 3.0 / 61.0).abs() < 1e-7);
        assert!((fused[1].1 - 1.0 / 61.0).abs() < 1e-7);
    }

    #[test]
    fn rrf_input_order_irrelevant_for_ranks() {
        // Unsorted input list: ranks must come from scores, not positions.
        let l1 = list(&[("low", 1.0), ("high", 9.0)]);
        let lists = [RankedList { weight: 1.0, items: &l1 }];
        let fused = reciprocal_rank_fusion(&lists, DEFAULT_RRF_K, 10);
        assert_eq!(ids(&fused), vec!["high", "low"]);
        assert!((fused[0].1 - 1.0 / 61.0).abs() < 1e-7);
        assert!((fused[1].1 - 1.0 / 62.0).abs() < 1e-7);
    }

    #[test]
    fn weighted_minmax_hand_computed() {
        let vector_results = list(&[("a", 10.0), ("b", 0.0)]); // -> a=1.0, b=0.0
        let text_results = list(&[("b", 5.0), ("a", 5.0)]); // degenerate -> both 1.0
        let lists = [
            RankedList { weight: 2.0, items: &vector_results },
            RankedList { weight: 1.0, items: &text_results },
        ];
        let fused = weighted_fusion(&lists, ScoreNorm::MinMax, 10);
        let by_id: HashMap<_, _> = fused.iter().map(|(i, s)| (i.as_str(), *s)).collect();
        assert!((by_id["a"] - 3.0).abs() < 1e-6); // 2*1 + 1*1
        assert!((by_id["b"] - 1.0).abs() < 1e-6); // 2*0 + 1*1
        assert_eq!(ids(&fused), vec!["a", "b"]);
    }

    #[test]
    fn weighted_raw_scores() {
        let l1 = list(&[("a", 0.5), ("b", 0.25)]);
        let l2 = list(&[("b", 0.5)]);
        let lists = [
            RankedList { weight: 1.0, items: &l1 },
            RankedList { weight: 2.0, items: &l2 },
        ];
        let fused = weighted_fusion(&lists, ScoreNorm::None, 10);
        let by_id: HashMap<_, _> = fused.iter().map(|(i, s)| (i.as_str(), *s)).collect();
        assert!((by_id["a"] - 0.5).abs() < 1e-6);
        assert!((by_id["b"] - 1.25).abs() < 1e-6);
        assert_eq!(ids(&fused), vec!["b", "a"]);
    }

    #[test]
    fn deterministic_tiebreak_by_id() {
        let l1 = list(&[("z", 1.0), ("a", 1.0), ("m", 1.0)]);
        let lists = [RankedList { weight: 1.0, items: &l1 }];
        let fused = weighted_fusion(&lists, ScoreNorm::None, 10);
        assert_eq!(ids(&fused), vec!["a", "m", "z"]);
    }

    #[test]
    fn empty_and_topk() {
        let empty: Vec<(String, f32)> = Vec::new();
        let lists = [RankedList { weight: 1.0, items: empty.as_slice() }];
        assert!(weighted_fusion(&lists, ScoreNorm::MinMax, 10).is_empty());
        assert!(reciprocal_rank_fusion(&lists, DEFAULT_RRF_K, 10).is_empty());

        let l1 = list(&[("a", 3.0), ("b", 2.0), ("c", 1.0)]);
        let lists = [RankedList { weight: 1.0, items: &l1 }];
        let fused = weighted_fusion(&lists, ScoreNorm::None, 2);
        assert_eq!(ids(&fused), vec!["a", "b"]);
    }

    #[test]
    fn works_with_u32_ids() {
        let l1: Vec<(u32, f32)> = vec![(7, 2.0), (3, 5.0)];
        let lists = [RankedList { weight: 1.0, items: l1.as_slice() }];
        let fused = reciprocal_rank_fusion(&lists, DEFAULT_RRF_K, 10);
        assert_eq!(fused[0].0, 3);
        assert_eq!(fused[1].0, 7);
    }
}
