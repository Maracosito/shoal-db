//! Bounded top-k selection.
//!
//! All ranking paths (exact kNN, IVF, BM25, fusion) funnel candidates
//! through [`TopK`], a fixed-capacity selector built on a binary heap that
//! keeps the *worst* retained candidate on top so new candidates can be
//! rejected in O(1) and admitted in O(log k).
//!
//! Ordering is fully deterministic: candidates compare by score
//! (`f32::total_cmp`), and equal scores break ties toward the **smaller
//! document ordinal**. NaN scores are silently dropped — they can arise
//! from degenerate vectors (e.g. cosine against a zero vector) and must
//! never poison a result set.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::types::DocOrd;

/// A scored candidate. `score` is always "higher is better".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbor {
    pub ord: DocOrd,
    pub score: f32,
}

/// Compares two neighbors by rank: `Greater` means `a` ranks better than
/// `b` (higher score; ties broken toward the smaller ordinal).
pub fn rank_cmp(a: &Neighbor, b: &Neighbor) -> Ordering {
    a.score
        .total_cmp(&b.score)
        .then_with(|| b.ord.cmp(&a.ord))
}

/// Wrapper that orders the heap so its maximum is the *worst-ranked*
/// retained neighbor.
struct Worst(Neighbor);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse rank order: the heap max is the worst neighbor.
        rank_cmp(&self.0, &other.0).reverse()
    }
}

/// Fixed-capacity top-k selector.
#[derive(Debug)]
pub struct TopK {
    k: usize,
    heap: BinaryHeap<Worst>,
}

impl std::fmt::Debug for Worst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Worst({:?})", self.0)
    }
}

impl TopK {
    /// Create a selector that retains at most `k` candidates.
    pub fn new(k: usize) -> Self {
        TopK {
            k,
            heap: BinaryHeap::with_capacity(k.min(1 << 16)),
        }
    }

    /// Offer a candidate. NaN scores are ignored.
    pub fn push(&mut self, n: Neighbor) {
        if self.k == 0 || n.score.is_nan() {
            return;
        }
        if self.heap.len() < self.k {
            self.heap.push(Worst(n));
        } else if let Some(worst) = self.heap.peek() {
            if rank_cmp(&n, &worst.0) == Ordering::Greater {
                self.heap.pop();
                self.heap.push(Worst(n));
            }
        }
    }

    /// Number of retained candidates so far.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// True if nothing has been retained.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Once the selector is full, the score a new candidate must beat.
    /// `None` while fewer than `k` candidates are retained (everything is
    /// still admissible). Useful for scan pruning.
    pub fn threshold(&self) -> Option<f32> {
        if self.heap.len() < self.k {
            None
        } else {
            self.heap.peek().map(|w| w.0.score)
        }
    }

    /// Consume the selector and return the retained candidates, best first.
    pub fn into_sorted(self) -> Vec<Neighbor> {
        let mut v: Vec<Neighbor> = self.heap.into_iter().map(|w| w.0).collect();
        v.sort_by(|a, b| rank_cmp(b, a));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_best_k() {
        let mut t = TopK::new(3);
        for (ord, score) in [(1, 0.1), (2, 0.9), (3, 0.5), (4, 0.7), (5, 0.2)] {
            t.push(Neighbor { ord, score });
        }
        let out = t.into_sorted();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].ord, 2);
        assert_eq!(out[1].ord, 4);
        assert_eq!(out[2].ord, 3);
    }

    #[test]
    fn deterministic_tie_break_prefers_smaller_ord() {
        let mut t = TopK::new(2);
        for ord in [9, 3, 7, 1] {
            t.push(Neighbor { ord, score: 1.0 });
        }
        let out = t.into_sorted();
        assert_eq!(out[0].ord, 1);
        assert_eq!(out[1].ord, 3);
    }

    #[test]
    fn skips_nan_and_handles_k_zero() {
        let mut t = TopK::new(2);
        t.push(Neighbor { ord: 1, score: f32::NAN });
        t.push(Neighbor { ord: 2, score: 0.5 });
        let out = t.into_sorted();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ord, 2);

        let mut z = TopK::new(0);
        z.push(Neighbor { ord: 1, score: 1.0 });
        assert!(z.into_sorted().is_empty());
    }

    #[test]
    fn threshold_appears_when_full() {
        let mut t = TopK::new(2);
        assert_eq!(t.threshold(), None);
        t.push(Neighbor { ord: 1, score: 0.4 });
        assert_eq!(t.threshold(), None);
        t.push(Neighbor { ord: 2, score: 0.8 });
        assert_eq!(t.threshold(), Some(0.4));
        t.push(Neighbor { ord: 3, score: 0.6 });
        assert_eq!(t.threshold(), Some(0.6));
    }

    #[test]
    fn handles_negative_scores() {
        // Euclidean scores are negative squared distances.
        let mut t = TopK::new(2);
        t.push(Neighbor { ord: 1, score: -10.0 });
        t.push(Neighbor { ord: 2, score: -1.0 });
        t.push(Neighbor { ord: 3, score: -5.0 });
        let out = t.into_sorted();
        assert_eq!(out[0].ord, 2);
        assert_eq!(out[1].ord, 3);
    }
}
