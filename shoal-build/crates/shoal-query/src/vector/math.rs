//! Distance/similarity primitives.
//!
//! Inner loops are written with four independent accumulators so LLVM can
//! auto-vectorize them on any target without `unsafe` or intrinsics. For
//! the dimensions typical of embedding models (256–3072) this is within a
//! small constant factor of hand-written SIMD and keeps the crate portable.

use serde::{Deserialize, Serialize};

use crate::error::{QueryError, Result};

/// Distance metric for dense-vector search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    /// Cosine similarity. Score = cos(a, b) ∈ [-1, 1].
    #[default]
    Cosine,
    /// Inner product (maximum inner product search). Score = a · b.
    Dot,
    /// Euclidean distance. Score = -‖a - b‖² (rank-equivalent to -‖a - b‖).
    Euclidean,
}

impl Metric {
    /// Compact tag used in serialized index headers.
    pub fn as_u8(self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::Dot => 1,
            Metric::Euclidean => 2,
        }
    }

    /// Inverse of [`Metric::as_u8`].
    pub fn from_u8(v: u8) -> Result<Metric> {
        match v {
            0 => Ok(Metric::Cosine),
            1 => Ok(Metric::Dot),
            2 => Ok(Metric::Euclidean),
            other => Err(QueryError::InvalidIndexFormat(format!(
                "unknown metric tag {other}"
            ))),
        }
    }
}

/// Dot product with four independent accumulators.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let n = a.len();
    let chunks = n / 4;
    let (mut s0, mut s1, mut s2, mut s3) = (0f32, 0f32, 0f32, 0f32);
    for i in 0..chunks {
        let j = i * 4;
        s0 += a[j] * b[j];
        s1 += a[j + 1] * b[j + 1];
        s2 += a[j + 2] * b[j + 2];
        s3 += a[j + 3] * b[j + 3];
    }
    let mut s = (s0 + s1) + (s2 + s3);
    for j in (chunks * 4)..n {
        s += a[j] * b[j];
    }
    s
}

/// Squared Euclidean distance.
pub fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let n = a.len();
    let chunks = n / 4;
    let (mut s0, mut s1, mut s2, mut s3) = (0f32, 0f32, 0f32, 0f32);
    for i in 0..chunks {
        let j = i * 4;
        let d0 = a[j] - b[j];
        let d1 = a[j + 1] - b[j + 1];
        let d2 = a[j + 2] - b[j + 2];
        let d3 = a[j + 3] - b[j + 3];
        s0 += d0 * d0;
        s1 += d1 * d1;
        s2 += d2 * d2;
        s3 += d3 * d3;
    }
    let mut s = (s0 + s1) + (s2 + s3);
    for j in (chunks * 4)..n {
        let d = a[j] - b[j];
        s += d * d;
    }
    s
}

/// Euclidean norm.
pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// Normalize a vector in place. Zero vectors are left untouched.
pub fn normalize_in_place(v: &mut [f32]) {
    let n = norm(v);
    if n > 0.0 && n.is_finite() {
        let inv = 1.0 / n;
        for x in v.iter_mut() {
            *x *= inv;
        }
    }
}

/// Return a normalized copy of a vector.
pub fn normalized(v: &[f32]) -> Vec<f32> {
    let mut out = v.to_vec();
    normalize_in_place(&mut out);
    out
}

/// Cosine similarity; returns 0.0 if either vector has zero norm.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot(a, b) / (na * nb)
}

/// Higher-is-better score under the given metric.
pub fn score(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => cosine_similarity(a, b),
        Metric::Dot => dot(a, b),
        Metric::Euclidean => -l2_sq(a, b),
    }
}

/// Convert an internal score back into a user-facing distance:
/// cosine distance (`1 - cos`), negated inner product, or Euclidean
/// distance (`sqrt` of the stored squared distance).
pub fn distance_from_score(metric: Metric, score: f32) -> f32 {
    match metric {
        Metric::Cosine => 1.0 - score,
        Metric::Dot => -score,
        Metric::Euclidean => (-score).max(0.0).sqrt(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_known_values() {
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = [5.0, 4.0, 3.0, 2.0, 1.0];
        // 5 + 8 + 9 + 8 + 5 = 35
        assert!((dot(&a, &b) - 35.0).abs() < 1e-6);
        assert_eq!(dot(&[], &[]), 0.0);
    }

    #[test]
    fn l2_known_values() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 2.0, 2.0];
        assert!((l2_sq(&a, &b) - 9.0).abs() < 1e-6);
        assert!((norm(&b) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_known_values() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        let c = [1.0, 1.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
        let expect = 1.0 / 2f32.sqrt();
        assert!((cosine_similarity(&a, &c) - expect).abs() < 1e-6);
        // zero vector is defined as similarity 0, not NaN
        assert_eq!(cosine_similarity(&a, &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn normalize_unit_norm() {
        let mut v = vec![3.0, 4.0];
        normalize_in_place(&mut v);
        assert!((norm(&v) - 1.0).abs() < 1e-6);
        // zero vector untouched
        let mut z = vec![0.0, 0.0];
        normalize_in_place(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn scores_are_higher_is_better() {
        let q = [1.0, 0.0];
        let near = [0.9, 0.1];
        let far = [-1.0, 0.0];
        for metric in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
            assert!(
                score(metric, &q, &near) > score(metric, &q, &far),
                "metric {metric:?} should prefer the near vector"
            );
        }
    }

    #[test]
    fn distance_round_trip() {
        let q = [1.0, 2.0];
        let v = [4.0, 6.0];
        let s = score(Metric::Euclidean, &q, &v);
        assert!((distance_from_score(Metric::Euclidean, s) - 5.0).abs() < 1e-5);
        let s = score(Metric::Cosine, &q, &q);
        assert!(distance_from_score(Metric::Cosine, s).abs() < 1e-6);
    }

    #[test]
    fn metric_tag_round_trip() {
        for m in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
            assert_eq!(Metric::from_u8(m.as_u8()).unwrap(), m);
        }
        assert!(Metric::from_u8(99).is_err());
    }

    #[test]
    fn metric_serde_names() {
        assert_eq!(serde_json::to_string(&Metric::Cosine).unwrap(), "\"cosine\"");
        let m: Metric = serde_json::from_str("\"euclidean\"").unwrap();
        assert_eq!(m, Metric::Euclidean);
    }
}
