//! Sorted document-id sets with merge-based set algebra.
//!
//! A [`DocSet`] is a strictly-increasing list of `u32` document ids within a
//! single segment. It is the currency of the filter engine: attribute-index
//! lookups produce `DocSet`s, and boolean filter operators combine them with
//! linear-time sorted merges. The representation is deliberately simple —
//! a sorted `Vec<u32>` — which is compact, cache-friendly, cheap to
//! delta-encode for persistence, and trivially correct. Segments in Shoal are
//! bounded in size by compaction, so a roaring-style compressed bitmap is not
//! required for v1 (and can be swapped in behind this API later).

use crate::codec::{CodecError, Reader, Writer};

/// An immutable sorted set of document ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocSet {
    ids: Vec<u32>,
}

impl DocSet {
    /// The empty set.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a set from an already strictly-increasing sorted vector.
    ///
    /// Debug builds assert the invariant; release builds trust the caller.
    pub fn from_sorted(ids: Vec<u32>) -> Self {
        debug_assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must be strictly increasing"
        );
        Self { ids }
    }

    /// Builds a set from arbitrary ids; sorts and deduplicates.
    pub fn from_unsorted(mut ids: Vec<u32>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self { ids }
    }

    /// The full universe `{0, 1, …, n-1}`.
    pub fn full(n: u32) -> Self {
        Self {
            ids: (0..n).collect(),
        }
    }

    /// Number of ids in the set.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Membership test (binary search).
    pub fn contains(&self, id: u32) -> bool {
        self.ids.binary_search(&id).is_ok()
    }

    /// Iterates ids in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.ids.iter().copied()
    }

    /// Borrows the underlying sorted slice.
    pub fn as_slice(&self) -> &[u32] {
        &self.ids
    }

    /// Consumes the set, returning the sorted vector.
    pub fn into_vec(self) -> Vec<u32> {
        self.ids
    }

    /// Set union (sorted merge).
    pub fn union(&self, other: &DocSet) -> DocSet {
        let (a, b) = (&self.ids, &other.ids);
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => {
                    out.push(a[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(b[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push(a[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        DocSet { ids: out }
    }

    /// Set intersection (sorted merge).
    pub fn intersect(&self, other: &DocSet) -> DocSet {
        let (a, b) = (&self.ids, &other.ids);
        let mut out = Vec::with_capacity(a.len().min(b.len()));
        let (mut i, mut j) = (0usize, 0usize);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(a[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        DocSet { ids: out }
    }

    /// Set difference `self \ other` (sorted merge).
    pub fn difference(&self, other: &DocSet) -> DocSet {
        let (a, b) = (&self.ids, &other.ids);
        let mut out = Vec::with_capacity(a.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                std::cmp::Ordering::Less => {
                    out.push(a[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        DocSet { ids: out }
    }

    /// Set complement relative to `universe`: `universe \ self`.
    pub fn complement(&self, universe: &DocSet) -> DocSet {
        universe.difference(self)
    }

    /// Serializes the set as a delta-encoded varint list.
    pub fn encode(&self, w: &mut Writer) {
        w.put_u32_deltas(&self.ids);
    }

    /// Deserializes a set written by [`DocSet::encode`].
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            ids: r.get_u32_deltas()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Reader, Writer};
    use std::collections::BTreeSet;

    /// Tiny deterministic xorshift64 generator so tests need no external RNG.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.max(1))
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n
        }
    }

    fn random_set(rng: &mut Rng, max_id: u64, size: u64) -> (DocSet, BTreeSet<u32>) {
        let mut model = BTreeSet::new();
        for _ in 0..size {
            model.insert(rng.below(max_id) as u32);
        }
        let set = DocSet::from_unsorted(model.iter().copied().collect());
        (set, model)
    }

    #[test]
    fn from_unsorted_sorts_and_dedups() {
        let s = DocSet::from_unsorted(vec![5, 1, 5, 3, 1]);
        assert_eq!(s.as_slice(), &[1, 3, 5]);
        assert!(s.contains(3));
        assert!(!s.contains(2));
    }

    #[test]
    fn ops_match_btreeset_model() {
        let mut rng = Rng::new(42);
        for _ in 0..50 {
            let (a, ma) = random_set(&mut rng, 200, 60);
            let (b, mb) = random_set(&mut rng, 200, 60);

            let union: Vec<u32> = ma.union(&mb).copied().collect();
            let inter: Vec<u32> = ma.intersection(&mb).copied().collect();
            let diff: Vec<u32> = ma.difference(&mb).copied().collect();

            assert_eq!(a.union(&b).as_slice(), union.as_slice());
            assert_eq!(a.intersect(&b).as_slice(), inter.as_slice());
            assert_eq!(a.difference(&b).as_slice(), diff.as_slice());
        }
    }

    #[test]
    fn complement_laws() {
        let universe = DocSet::full(100);
        let a = DocSet::from_unsorted(vec![1, 5, 7, 50, 99]);
        let not_a = a.complement(&universe);
        assert_eq!(a.intersect(&not_a), DocSet::empty());
        assert_eq!(a.union(&not_a), universe);
        assert_eq!(not_a.complement(&universe), a);
    }

    #[test]
    fn empty_identities() {
        let a = DocSet::from_unsorted(vec![3, 9]);
        let e = DocSet::empty();
        assert_eq!(a.union(&e), a);
        assert_eq!(a.intersect(&e), e);
        assert_eq!(a.difference(&e), a);
        assert_eq!(e.difference(&a), e);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut rng = Rng::new(7);
        for _ in 0..20 {
            let (a, _) = random_set(&mut rng, 1_000_000, 500);
            let mut w = Writer::new();
            a.encode(&mut w);
            let bytes = w.into_bytes();
            let mut r = Reader::new(&bytes);
            assert_eq!(DocSet::decode(&mut r).unwrap(), a);
            assert!(r.is_empty());
        }
        // Empty set round-trips too.
        let mut w = Writer::new();
        DocSet::empty().encode(&mut w);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(DocSet::decode(&mut r).unwrap(), DocSet::empty());
    }
}
