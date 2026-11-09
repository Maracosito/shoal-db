//! Exact (brute-force) k-nearest-neighbor search.
//!
//! This is both the correctness baseline that ANN recall is measured
//! against and the execution path the planner picks for small namespaces
//! or highly selective filters, where a linear scan over the (cached)
//! vector columns is cheaper than probing an IVF index.

use roaring::RoaringBitmap;

use crate::error::{QueryError, Result};
use crate::topk::{Neighbor, TopK};
use crate::types::DocOrd;
use crate::vector::math::{self, Metric};

/// Exact kNN over a flat, column-oriented vector block: `ords[i]`'s vector
/// occupies `data[i*dim .. (i+1)*dim]`. Pass `allow` to restrict the scan
/// to a pre-computed filter bitmap.
pub fn search_flat(
    metric: Metric,
    query: &[f32],
    dim: usize,
    ords: &[DocOrd],
    data: &[f32],
    k: usize,
    allow: Option<&RoaringBitmap>,
) -> Result<Vec<Neighbor>> {
    if dim == 0 {
        return Err(QueryError::InvalidArgument("dim must be > 0".into()));
    }
    if query.len() != dim {
        return Err(QueryError::DimensionMismatch {
            expected: dim,
            got: query.len(),
        });
    }
    if data.len() != ords.len() * dim {
        return Err(QueryError::InvalidArgument(format!(
            "vector block size mismatch: {} ords * dim {} != {} floats",
            ords.len(),
            dim,
            data.len()
        )));
    }
    let mut topk = TopK::new(k);
    for (i, &ord) in ords.iter().enumerate() {
        if let Some(bm) = allow {
            if !bm.contains(ord) {
                continue;
            }
        }
        let v = &data[i * dim..(i + 1) * dim];
        topk.push(Neighbor {
            ord,
            score: math::score(metric, query, v),
        });
    }
    Ok(topk.into_sorted())
}

/// Exact kNN over an arbitrary iterator of `(ordinal, vector)` pairs.
/// Each vector must match the query's dimensionality.
pub fn search_pairs<'a, I>(
    metric: Metric,
    query: &[f32],
    items: I,
    k: usize,
    allow: Option<&RoaringBitmap>,
) -> Result<Vec<Neighbor>>
where
    I: IntoIterator<Item = (DocOrd, &'a [f32])>,
{
    let mut topk = TopK::new(k);
    for (ord, v) in items {
        if v.len() != query.len() {
            return Err(QueryError::DimensionMismatch {
                expected: query.len(),
                got: v.len(),
            });
        }
        if let Some(bm) = allow {
            if !bm.contains(ord) {
                continue;
            }
        }
        topk.push(Neighbor {
            ord,
            score: math::score(metric, query, v),
        });
    }
    Ok(topk.into_sorted())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn random_block(rng: &mut StdRng, n: usize, dim: usize) -> (Vec<DocOrd>, Vec<f32>) {
        let ords: Vec<DocOrd> = (0..n as u32).collect();
        let data: Vec<f32> = (0..n * dim).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
        (ords, data)
    }

    /// Naive reference: score everything, full sort, take k.
    fn reference(
        metric: Metric,
        query: &[f32],
        dim: usize,
        ords: &[DocOrd],
        data: &[f32],
        k: usize,
    ) -> Vec<DocOrd> {
        let mut scored: Vec<(f32, DocOrd)> = ords
            .iter()
            .enumerate()
            .map(|(i, &o)| (math::score(metric, query, &data[i * dim..(i + 1) * dim]), o))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(k).map(|(_, o)| o).collect()
    }

    #[test]
    fn matches_naive_full_sort_for_all_metrics() {
        let mut rng = StdRng::seed_from_u64(7);
        let dim = 17; // intentionally not a multiple of 4
        let (ords, data) = random_block(&mut rng, 300, dim);
        let query: Vec<f32> = (0..dim).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
        for metric in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
            let got: Vec<DocOrd> = search_flat(metric, &query, dim, &ords, &data, 10, None)
                .unwrap()
                .into_iter()
                .map(|n| n.ord)
                .collect();
            let want = reference(metric, &query, dim, &ords, &data, 10);
            assert_eq!(got, want, "metric {metric:?}");
        }
    }

    #[test]
    fn respects_allow_bitmap() {
        let mut rng = StdRng::seed_from_u64(13);
        let dim = 8;
        let (ords, data) = random_block(&mut rng, 100, dim);
        let query: Vec<f32> = (0..dim).map(|_| rng.gen()).collect();
        let mut allow = RoaringBitmap::new();
        for o in ords.iter().filter(|o| *o % 3 == 0) {
            allow.insert(*o);
        }
        let out =
            search_flat(Metric::Euclidean, &query, dim, &ords, &data, 20, Some(&allow)).unwrap();
        assert!(!out.is_empty());
        assert!(out.iter().all(|n| n.ord % 3 == 0));
    }

    #[test]
    fn rejects_dimension_mismatch() {
        let ords = vec![0u32];
        let data = vec![1.0f32, 2.0];
        let err = search_flat(Metric::Dot, &[1.0, 2.0, 3.0], 2, &ords, &data, 5, None);
        assert!(matches!(
            err,
            Err(QueryError::DimensionMismatch { expected: 2, got: 3 })
        ));
        let err = search_pairs(Metric::Dot, &[1.0, 2.0], [(0u32, &[1.0f32][..])], 5, None);
        assert!(matches!(err, Err(QueryError::DimensionMismatch { .. })));
    }

    #[test]
    fn k_larger_than_n_returns_all() {
        let ords = vec![0u32, 1, 2];
        let data = vec![1.0f32, 0.0, 0.0, 1.0, 1.0, 1.0];
        let out = search_flat(Metric::Dot, &[1.0, 1.0], 2, &ords, &data, 50, None).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].ord, 2); // dot = 2.0
    }
}
