//! Lloyd's k-means with k-means++ seeding, used to train IVF centroids.
//!
//! Deterministic given a seed, robust against empty clusters (an empty
//! cluster steals the point that is currently farthest from its assigned
//! centroid), and tolerant of `k > n` (clamped to `n`).

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::error::{QueryError, Result};
use crate::vector::math::l2_sq;

/// Training configuration.
#[derive(Debug, Clone)]
pub struct KMeansConfig {
    /// Number of clusters requested (clamped to the number of points).
    pub k: usize,
    /// Maximum Lloyd iterations.
    pub max_iters: usize,
    /// RNG seed for reproducible training.
    pub seed: u64,
}

impl Default for KMeansConfig {
    fn default() -> Self {
        KMeansConfig {
            k: 16,
            max_iters: 12,
            seed: 0x5EED_5EED,
        }
    }
}

/// Training output.
#[derive(Debug, Clone)]
pub struct KMeansResult {
    /// Row-major centroid matrix, `k * dim` floats.
    pub centroids: Vec<f32>,
    /// Cluster assignment per input point.
    pub assignments: Vec<u32>,
    /// Effective number of clusters (may be smaller than requested).
    pub k: usize,
    /// Dimensionality.
    pub dim: usize,
    /// Lloyd iterations actually run.
    pub iterations: usize,
    /// Sum of squared distances of points to their centroids.
    pub inertia: f32,
}

fn pt(data: &[f32], dim: usize, i: usize) -> &[f32] {
    &data[i * dim..(i + 1) * dim]
}

/// Train k-means on a row-major `n x dim` matrix.
pub fn train(data: &[f32], dim: usize, cfg: &KMeansConfig) -> Result<KMeansResult> {
    if dim == 0 {
        return Err(QueryError::InvalidArgument("kmeans: dim must be > 0".into()));
    }
    if data.is_empty() || data.len() % dim != 0 {
        return Err(QueryError::InvalidArgument(format!(
            "kmeans: data length {} is not a non-zero multiple of dim {}",
            data.len(),
            dim
        )));
    }
    let n = data.len() / dim;
    let k = cfg.k.clamp(1, n);
    let mut rng = StdRng::seed_from_u64(cfg.seed);

    // ---- k-means++ seeding -------------------------------------------------
    let mut centroids: Vec<f32> = Vec::with_capacity(k * dim);
    let first = rng.gen_range(0..n);
    centroids.extend_from_slice(pt(data, dim, first));
    let mut d2 = vec![f32::MAX; n];
    for c in 1..k {
        let last = &centroids[(c - 1) * dim..c * dim];
        for i in 0..n {
            let d = l2_sq(pt(data, dim, i), last);
            if d < d2[i] {
                d2[i] = d;
            }
        }
        let total: f64 = d2.iter().map(|&x| x as f64).sum();
        let chosen = if total <= 0.0 || !total.is_finite() {
            rng.gen_range(0..n)
        } else {
            let mut target = rng.gen::<f64>() * total;
            let mut chosen = n - 1;
            for (i, &d) in d2.iter().enumerate() {
                target -= d as f64;
                if target <= 0.0 {
                    chosen = i;
                    break;
                }
            }
            chosen
        };
        centroids.extend_from_slice(pt(data, dim, chosen));
    }

    // ---- Lloyd iterations --------------------------------------------------
    let mut assignments = vec![0u32; n];
    let mut dists = vec![0f32; n];
    let mut iterations = 0usize;
    let mut inertia = 0f32;

    for iter in 0..cfg.max_iters.max(1) {
        iterations = iter + 1;
        // Assignment step.
        let mut changed = 0usize;
        inertia = 0.0;
        for i in 0..n {
            let p = pt(data, dim, i);
            let mut best = 0u32;
            let mut best_d = f32::MAX;
            for c in 0..k {
                let d = l2_sq(p, &centroids[c * dim..(c + 1) * dim]);
                if d < best_d {
                    best_d = d;
                    best = c as u32;
                }
            }
            if assignments[i] != best {
                changed += 1;
            }
            assignments[i] = best;
            dists[i] = best_d;
            inertia += best_d;
        }

        // Update step.
        let mut sums = vec![0f64; k * dim];
        let mut counts = vec![0usize; k];
        for i in 0..n {
            let c = assignments[i] as usize;
            counts[c] += 1;
            let p = pt(data, dim, i);
            for (j, &x) in p.iter().enumerate() {
                sums[c * dim + j] += x as f64;
            }
        }
        for c in 0..k {
            if counts[c] > 0 {
                let inv = 1.0 / counts[c] as f64;
                for j in 0..dim {
                    centroids[c * dim + j] = (sums[c * dim + j] * inv) as f32;
                }
            }
        }

        // Empty-cluster repair: each empty cluster steals the point that is
        // farthest from its current centroid (from a cluster with > 1 point).
        for c in 0..k {
            if counts[c] > 0 {
                continue;
            }
            let mut steal: Option<usize> = None;
            let mut steal_d = -1f32;
            for i in 0..n {
                if counts[assignments[i] as usize] > 1 && dists[i] > steal_d {
                    steal_d = dists[i];
                    steal = Some(i);
                }
            }
            if let Some(i) = steal {
                let old = assignments[i] as usize;
                counts[old] -= 1;
                counts[c] = 1;
                assignments[i] = c as u32;
                let p = pt(data, dim, i).to_vec();
                centroids[c * dim..(c + 1) * dim].copy_from_slice(&p);
                dists[i] = 0.0;
                changed += 1;
            }
        }

        if changed == 0 {
            break;
        }
    }

    Ok(KMeansResult {
        centroids,
        assignments,
        k,
        dim,
        iterations,
        inertia,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three well-separated 2-D clusters with deterministic jitter.
    fn blobs() -> (Vec<f32>, Vec<usize>) {
        let centers = [(0.0f32, 0.0f32), (10.0, 10.0), (-10.0, 10.0)];
        let mut data = Vec::new();
        let mut truth = Vec::new();
        for (ci, &(cx, cy)) in centers.iter().enumerate() {
            for s in 0..30 {
                // Small deterministic jitter in [-0.5, 0.5].
                let jx = ((s * 7 + ci * 3) % 11) as f32 / 11.0 - 0.5;
                let jy = ((s * 5 + ci * 13) % 11) as f32 / 11.0 - 0.5;
                data.push(cx + jx);
                data.push(cy + jy);
                truth.push(ci);
            }
        }
        (data, truth)
    }

    #[test]
    fn recovers_separated_clusters() {
        let (data, truth) = blobs();
        let cfg = KMeansConfig { k: 3, max_iters: 25, seed: 42 };
        let res = train(&data, 2, &cfg).unwrap();
        assert_eq!(res.k, 3);
        // Every ground-truth cluster must map to exactly one label,
        // and labels must not be shared between ground-truth clusters.
        let mut label_of = [usize::MAX; 3];
        for (i, &t) in truth.iter().enumerate() {
            let a = res.assignments[i] as usize;
            if label_of[t] == usize::MAX {
                label_of[t] = a;
            }
            assert_eq!(label_of[t], a, "cluster {t} split across labels");
        }
        assert_ne!(label_of[0], label_of[1]);
        assert_ne!(label_of[1], label_of[2]);
        assert_ne!(label_of[0], label_of[2]);
        // Tight clusters => tiny inertia per point.
        assert!(res.inertia / truth.len() as f32 <
            1.0, "inertia too high: {}", res.inertia);
    }

    #[test]
    fn deterministic_given_seed() {
        let (data, _) = blobs();
        let cfg = KMeansConfig { k: 3, max_iters: 25, seed: 99 };
        let a = train(&data, 2, &cfg).unwrap();
        let b = train(&data, 2, &cfg).unwrap();
        assert_eq!(a.centroids, b.centroids);
        assert_eq!(a.assignments, b.assignments);
    }

    #[test]
    fn clamps_k_to_n() {
        let data = vec![0.0f32, 0.0, 1.0, 1.0]; // two 2-D points
        let cfg = KMeansConfig { k: 10, max_iters: 5, seed: 1 };
        let res = train(&data, 2, &cfg).unwrap();
        assert_eq!(res.k, 2);
        assert_eq!(res.centroids.len(), 4);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(train(&[], 2, &KMeansConfig::default()).is_err());
        assert!(train(&[1.0, 2.0, 3.0], 2, &KMeansConfig::default()).is_err());
        assert!(train(&[1.0], 0, &KMeansConfig::default()).is_err());
    }
}
