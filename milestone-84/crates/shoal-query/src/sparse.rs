//! Sparse-vector scoring path.
//!
//! Sparse vectors (e.g. SPLADE or BM25-style learned term weights) are stored
//! as parallel `(indices, values)` arrays with strictly increasing `u32`
//! dimension ids. Scoring is the dot product between query and document
//! vectors.
//!
//! [`SparseIndex`] is an immutable dimension-major inverted structure over one
//! segment's documents (local `u32` ordinals, the same ordinal space as the
//! text and filter indexes). Like the other index formats in this crate it
//! serializes to a single blob that lives in object storage and is cached
//! locally.
//!
//! ## On-disk layout (version 1)
//!
//! ```text
//! magic      "SHSV" (4 bytes)
//! version    u32 LE
//! doc_count  u32 LE      -- number of distinct documents indexed
//! dim_count  u32 LE
//! per dim (ascending): dim u32 LE, posting_count u32 LE,
//!     per posting: uvarint(doc delta), value f32 LE
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use crate::wire::{self, Reader, WireError};

/// Magic bytes identifying a sparse-vector segment.
pub const SPARSE_MAGIC: [u8; 4] = *b"SHSV";
/// Current sparse segment format version.
pub const SPARSE_VERSION: u32 = 1;

/// Errors produced by the sparse-vector engine.
#[derive(Debug)]
pub enum SparseError {
    /// `indices` and `values` have different lengths.
    LengthMismatch { indices: usize, values: usize },
    /// Indices are not strictly increasing.
    UnsortedIndices,
    /// A value is NaN or infinite.
    NonFiniteValue,
    /// The same document ordinal was added twice.
    DuplicateDoc(u32),
    /// Low-level decode failure.
    Wire(WireError),
    /// The blob does not start with the sparse magic.
    BadMagic,
    /// The blob has an unknown format version.
    UnsupportedVersion(u32),
    /// Structural corruption with a description.
    Corrupt(String),
}

impl fmt::Display for SparseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SparseError::LengthMismatch { indices, values } => write!(
                f,
                "sparse vector length mismatch: {indices} indices vs {values} values"
            ),
            SparseError::UnsortedIndices => {
                write!(f, "sparse vector indices must be strictly increasing")
            }
            SparseError::NonFiniteValue => write!(f, "sparse vector value is NaN or infinite"),
            SparseError::DuplicateDoc(d) => write!(f, "document {d} added twice to sparse index"),
            SparseError::Wire(e) => write!(f, "wire decode error: {e}"),
            SparseError::BadMagic => write!(f, "not a sparse-vector segment (bad magic)"),
            SparseError::UnsupportedVersion(v) => {
                write!(f, "unsupported sparse segment version {v}")
            }
            SparseError::Corrupt(msg) => write!(f, "corrupt sparse segment: {msg}"),
        }
    }
}

impl std::error::Error for SparseError {}

impl From<WireError> for SparseError {
    fn from(e: WireError) -> Self {
        SparseError::Wire(e)
    }
}

// ---------------------------------------------------------------------------
// SparseVector
// ---------------------------------------------------------------------------

/// A validated sparse vector: strictly increasing dimension ids with finite
/// values.
#[derive(Debug, Clone, PartialEq)]
pub struct SparseVector {
    indices: Vec<u32>,
    values: Vec<f32>,
}

impl SparseVector {
    /// Construct from pre-sorted parallel arrays. Validates lengths, strict
    /// ordering, and finiteness.
    pub fn new(indices: Vec<u32>, values: Vec<f32>) -> Result<Self, SparseError> {
        if indices.len() != values.len() {
            return Err(SparseError::LengthMismatch {
                indices: indices.len(),
                values: values.len(),
            });
        }
        for w in indices.windows(2) {
            if w[0] >= w[1] {
                return Err(SparseError::UnsortedIndices);
            }
        }
        if values.iter().any(|v| !v.is_finite()) {
            return Err(SparseError::NonFiniteValue);
        }
        Ok(SparseVector { indices, values })
    }

    /// Construct from arbitrary `(dimension, value)` pairs: sorts by
    /// dimension, sums duplicate dimensions, and drops exact-zero entries.
    pub fn from_pairs(mut pairs: Vec<(u32, f32)>) -> Result<Self, SparseError> {
        pairs.sort_unstable_by_key(|p| p.0);
        let mut indices = Vec::with_capacity(pairs.len());
        let mut values: Vec<f32> = Vec::with_capacity(pairs.len());
        for (dim, val) in pairs {
            if !val.is_finite() {
                return Err(SparseError::NonFiniteValue);
            }
            if let Some(&last) = indices.last() {
                if last == dim {
                    *values.last_mut().expect("parallel arrays") += val;
                    continue;
                }
            }
            indices.push(dim);
            values.push(val);
        }
        // Drop exact zeros (including duplicates that cancelled out).
        let mut out_i = Vec::with_capacity(indices.len());
        let mut out_v = Vec::with_capacity(values.len());
        for (i, v) in indices.into_iter().zip(values) {
            if v != 0.0 {
                out_i.push(i);
                out_v.push(v);
            }
        }
        SparseVector::new(out_i, out_v)
    }

    /// Number of non-zero entries.
    pub fn nnz(&self) -> usize {
        self.indices.len()
    }

    /// Dimension ids (strictly increasing).
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// Values parallel to [`SparseVector::indices`].
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Iterate `(dimension, value)` pairs in ascending dimension order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.indices.iter().copied().zip(self.values.iter().copied())
    }

    /// Sparse dot product via sorted-merge walk.
    pub fn dot(&self, other: &SparseVector) -> f32 {
        let mut i = 0;
        let mut j = 0;
        let mut sum = 0.0f32;
        while i < self.indices.len() && j < other.indices.len() {
            match self.indices[i].cmp(&other.indices[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    sum += self.values[i] * other.values[j];
                    i += 1;
                    j += 1;
                }
            }
        }
        sum
    }
}

// ---------------------------------------------------------------------------
// SparseIndex
// ---------------------------------------------------------------------------

/// Builds a [`SparseIndex`] from `(doc, vector)` pairs.
#[derive(Debug, Default)]
pub struct SparseIndexBuilder {
    postings: BTreeMap<u32, Vec<(u32, f32)>>,
    seen: HashSet<u32>,
}

impl SparseIndexBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the sparse vector for document ordinal `doc`. Each ordinal may be
    /// added at most once.
    pub fn add(&mut self, doc: u32, vector: &SparseVector) -> Result<(), SparseError> {
        if !self.seen.insert(doc) {
            return Err(SparseError::DuplicateDoc(doc));
        }
        for (dim, val) in vector.iter() {
            self.postings.entry(dim).or_default().push((doc, val));
        }
        Ok(())
    }

    /// Finish building.
    pub fn build(self) -> SparseIndex {
        let mut postings = self.postings;
        for list in postings.values_mut() {
            list.sort_unstable_by_key(|p| p.0);
        }
        SparseIndex {
            postings,
            doc_count: self.seen.len() as u32,
        }
    }
}

/// An immutable dimension-major sparse-vector index over one segment.
#[derive(Debug)]
pub struct SparseIndex {
    /// dim -> ascending (doc, value) postings.
    postings: BTreeMap<u32, Vec<(u32, f32)>>,
    doc_count: u32,
}

impl SparseIndex {
    /// Number of distinct documents indexed.
    pub fn doc_count(&self) -> u32 {
        self.doc_count
    }

    /// Number of distinct dimensions with at least one posting.
    pub fn dim_count(&self) -> usize {
        self.postings.len()
    }

    /// Score `query` against all indexed documents by dot product and return
    /// the `top_k` highest-scoring documents (score descending, doc-id
    /// ascending tiebreak). `filter` is an optional doc-ordinal predicate.
    ///
    /// Documents whose accumulated dot product is exactly `0.0` are omitted.
    pub fn search(
        &self,
        query: &SparseVector,
        top_k: usize,
        filter: Option<&dyn Fn(u32) -> bool>,
    ) -> Vec<(u32, f32)> {
        let mut acc: HashMap<u32, f32> = HashMap::new();
        for (dim, qval) in query.iter() {
            let Some(list) = self.postings.get(&dim) else {
                continue;
            };
            for &(doc, dval) in list {
                if let Some(f) = filter {
                    if !f(doc) {
                        continue;
                    }
                }
                *acc.entry(doc).or_insert(0.0) += qval * dval;
            }
        }
        let mut results: Vec<(u32, f32)> = acc.into_iter().filter(|&(_, s)| s != 0.0).collect();
        results.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if top_k < results.len() {
            results.truncate(top_k);
        }
        results
    }

    /// Serialize the index into a single blob.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SPARSE_MAGIC);
        wire::put_u32(&mut out, SPARSE_VERSION);
        wire::put_u32(&mut out, self.doc_count);
        wire::put_u32(&mut out, self.postings.len() as u32);
        for (&dim, list) in &self.postings {
            wire::put_u32(&mut out, dim);
            wire::put_u32(&mut out, list.len() as u32);
            let mut prev = 0u32;
            let mut first = true;
            for &(doc, val) in list {
                let delta = if first { doc } else { doc - prev };
                wire::put_uvarint(&mut out, delta as u64);
                wire::put_f32(&mut out, val);
                prev = doc;
                first = false;
            }
        }
        out
    }

    /// Deserialize an index, validating structure. Returns an error (never
    /// panics) on truncated or corrupt input.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SparseError> {
        let mut r = Reader::new(bytes);
        let magic = r.take(4)?;
        if magic != SPARSE_MAGIC {
            return Err(SparseError::BadMagic);
        }
        let version = r.read_u32()?;
        if version != SPARSE_VERSION {
            return Err(SparseError::UnsupportedVersion(version));
        }
        let doc_count = r.read_u32()?;
        let dim_count = r.read_u32()?;
        let mut postings: BTreeMap<u32, Vec<(u32, f32)>> = BTreeMap::new();
        let mut prev_dim: Option<u32> = None;
        for _ in 0..dim_count {
            let dim = r.read_u32()?;
            if let Some(p) = prev_dim {
                if dim <= p {
                    return Err(SparseError::Corrupt(format!(
                        "dimension ids not strictly increasing at {dim}"
                    )));
                }
            }
            prev_dim = Some(dim);
            let count = r.read_u32()?;
            // Each posting needs at least 5 bytes (1 varint byte + f32).
            if count as u64 * 5 > r.remaining() as u64 {
                return Err(SparseError::Corrupt(format!(
                    "posting list for dim {dim} claims {count} entries, only {} bytes remain",
                    r.remaining()
                )));
            }
            let mut list = Vec::with_capacity(count as usize);
            let mut prev = 0u32;
            let mut first = true;
            for _ in 0..count {
                let delta = r.read_uvarint_u32()?;
                if !first && delta == 0 {
                    return Err(SparseError::Corrupt(format!(
                        "duplicate doc id in posting list for dim {dim}"
                    )));
                }
                let doc = if first { delta } else { prev + delta };
                let val = r.read_f32()?;
                list.push((doc, val));
                prev = doc;
                first = false;
            }
            postings.insert(dim, list);
        }
        Ok(SparseIndex {
            postings,
            doc_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(pairs: &[(u32, f32)]) -> SparseVector {
        SparseVector::from_pairs(pairs.to_vec()).unwrap()
    }

    /// Dense reference dot product.
    fn dense_dot(a: &SparseVector, b: &SparseVector, dims: usize) -> f32 {
        let mut da = vec![0.0f32; dims];
        let mut db = vec![0.0f32; dims];
        for (i, v) in a.iter() {
            da[i as usize] = v;
        }
        for (i, v) in b.iter() {
            db[i as usize] = v;
        }
        da.iter().zip(&db).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn dot_matches_dense_reference() {
        let a = sv(&[(0, 1.0), (3, 2.0), (7, -1.5), (9, 0.5)]);
        let b = sv(&[(1, 4.0), (3, 3.0), (7, 2.0)]);
        let expected = dense_dot(&a, &b, 16);
        assert!((a.dot(&b) - expected).abs() < 1e-6);
        assert!((b.dot(&a) - expected).abs() < 1e-6);
        // Disjoint vectors.
        let c = sv(&[(100, 1.0)]);
        assert_eq!(a.dot(&c), 0.0);
    }

    #[test]
    fn from_pairs_sorts_merges_and_drops_zeros() {
        let v = SparseVector::from_pairs(vec![(5, 1.0), (2, 3.0), (5, 2.0), (8, 0.0)]).unwrap();
        assert_eq!(v.indices(), &[2, 5]);
        assert_eq!(v.values(), &[3.0, 3.0]);
    }

    #[test]
    fn validation_errors() {
        assert!(matches!(
            SparseVector::new(vec![0, 1], vec![1.0]),
            Err(SparseError::LengthMismatch { .. })
        ));
        assert!(matches!(
            SparseVector::new(vec![3, 1], vec![1.0, 2.0]),
            Err(SparseError::UnsortedIndices)
        ));
        assert!(matches!(
            SparseVector::new(vec![1, 1], vec![1.0, 2.0]),
            Err(SparseError::UnsortedIndices)
        ));
        assert!(matches!(
            SparseVector::new(vec![0], vec![f32::NAN]),
            Err(SparseError::NonFiniteValue)
        ));
    }

    fn build_index(docs: &[(u32, SparseVector)]) -> SparseIndex {
        let mut b = SparseIndexBuilder::new();
        for (doc, v) in docs {
            b.add(*doc, v).unwrap();
        }
        b.build()
    }

    #[test]
    fn search_matches_bruteforce() {
        let docs = vec![
            (0, sv(&[(1, 1.0), (4, 2.0)])),
            (1, sv(&[(1, 3.0)])),
            (2, sv(&[(4, 1.0), (9, 5.0)])),
            (3, sv(&[(2, 7.0)])),
        ];
        let idx = build_index(&docs);
        let q = sv(&[(1, 2.0), (4, 1.0), (9, 0.5)]);
        let got = idx.search(&q, 10, None);
        // Brute force.
        let mut expected: Vec<(u32, f32)> = docs
            .iter()
            .map(|(d, v)| (*d, q.dot(v)))
            .filter(|&(_, s)| s != 0.0)
            .collect();
        expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        assert_eq!(got.len(), expected.len());
        for ((gd, gs), (ed, es)) in got.iter().zip(&expected) {
            assert_eq!(gd, ed);
            assert!((gs - es).abs() < 1e-6);
        }
        // top_k truncation and filter.
        assert_eq!(idx.search(&q, 1, None).len(), 1);
        let pred: &dyn Fn(u32) -> bool = &|d| d != expected[0].0;
        let filtered = idx.search(&q, 10, Some(pred));
        assert!(filtered.iter().all(|&(d, _)| d != expected[0].0));
    }

    #[test]
    fn duplicate_doc_rejected() {
        let mut b = SparseIndexBuilder::new();
        b.add(7, &sv(&[(0, 1.0)])).unwrap();
        assert!(matches!(
            b.add(7, &sv(&[(1, 1.0)])),
            Err(SparseError::DuplicateDoc(7))
        ));
    }

    #[test]
    fn roundtrip() {
        let docs = vec![
            (0, sv(&[(1, 1.0), (4, 2.0)])),
            (5, sv(&[(1, -3.0), (1000, 0.25)])),
        ];
        let idx = build_index(&docs);
        let bytes = idx.to_bytes();
        let idx2 = SparseIndex::from_bytes(&bytes).unwrap();
        assert_eq!(idx2.doc_count(), idx.doc_count());
        assert_eq!(idx2.dim_count(), idx.dim_count());
        let q = sv(&[(1, 1.0), (4, 1.0), (1000, 1.0)]);
        assert_eq!(idx.search(&q, 10, None), idx2.search(&q, 10, None));
    }

    #[test]
    fn corrupt_input_is_error_not_panic() {
        let idx = build_index(&[(0, sv(&[(1, 1.0)]))]);
        let mut bytes = idx.to_bytes();
        bytes[0] = b'X';
        assert!(matches!(
            SparseIndex::from_bytes(&bytes),
            Err(SparseError::BadMagic)
        ));
        let bytes = idx.to_bytes();
        assert!(SparseIndex::from_bytes(&bytes[..bytes.len() - 2]).is_err());
        let mut bytes = idx.to_bytes();
        bytes[4] = 99; // version
        assert!(matches!(
            SparseIndex::from_bytes(&bytes),
            Err(SparseError::UnsupportedVersion(99))
        ));
    }
}
