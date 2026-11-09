//! Query planning and execution.
//!
//! This module ties the individual retrieval primitives (exact kNN, IVF ANN,
//! BM25 full-text, sparse-vector scoring, and the metadata filter engine)
//! together behind a single request type, a cost-heuristic planner, and an
//! executor.
//!
//! # Design
//!
//! The planner and executor are deliberately decoupled from concrete segment
//! readers via the [`QuerySource`] trait. A `QuerySource` is "one queryable
//! view of a namespace": in production it is implemented by the serving layer
//! on top of object-storage-backed segments and the local cache hierarchy; in
//! tests it can be a trivial in-memory table. This keeps the planner pure
//! (it only consumes statistics) and makes the execution pipeline testable
//! without any storage backend.
//!
//! # Score convention
//!
//! Every [`Hit`] score is **higher-is-better**, regardless of metric. Sources
//! serving Euclidean queries must return a similarity-shaped score (e.g. the
//! negated distance, which is what `shoal-query`'s vector module produces) so
//! that fusion and top-k truncation can treat all legs uniformly.
//!
//! # Pipeline
//!
//! 1. [`request::QueryRequest`] is validated.
//! 2. [`planner::plan_query`] inspects index availability, corpus size, and
//!    filter selectivity hints and emits a [`planner::QueryPlan`]: a filter
//!    strategy (pre-filter via the attribute index, post-filter, or none),
//!    one retrieval *leg* per ranking signal (exact kNN, IVF with a chosen
//!    `nprobe`, full-text, sparse, or a pure filter scan), and an optional
//!    fusion stage (RRF or weighted score fusion).
//! 3. [`executor::execute_plan`] evaluates the filter, runs each leg
//!    (adaptively downgrading IVF to exact search when the allow-set turns
//!    out to be tiny), fuses, truncates to `top_k`, and materializes the
//!    selected attribute projection.

pub mod executor;
pub mod fuse;
pub mod planner;
pub mod request;

pub use executor::{execute, execute_multi, execute_plan, ExecutionStats, LegStats, QueryOutput, Row};
pub use fuse::{rrf_fuse, weighted_fuse};
pub use planner::{plan_query, FilterStrategy, FusionPlan, LegKind, PlanLeg, PlannerConfig, QueryPlan};
pub use request::{
    AnnMode, AnnOptions, FieldBoost, FusionSpec, Metric, MultiQuery, Projection, QueryRequest,
    SparseQuery, TextQuery, VectorQuery,
};

use std::fmt;

/// A single scored candidate produced by a retrieval leg.
///
/// `doc` is the namespace-local document id used by segment readers; the
/// source maps it back to the user-visible document id during
/// [`QuerySource::materialize`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub doc: u64,
    /// Higher is better. See the module docs for the score convention.
    pub score: f32,
}

/// Errors surfaced by planning and execution.
#[derive(Debug)]
pub enum ExecError {
    /// The request itself is malformed or unsatisfiable (e.g. `top_k == 0`,
    /// ANN forced on a field with no ANN index).
    InvalidRequest(String),
    /// The underlying source failed (storage error, decode error, ...).
    Source(String),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::InvalidRequest(m) => write!(f, "invalid query request: {m}"),
            ExecError::Source(m) => write!(f, "query source error: {m}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// A set of namespace-local document ids that a query is allowed to return.
///
/// Produced by evaluating the metadata filter against the attribute/filter
/// indexes; consumed by every retrieval leg as a pre-filter. Stored as a
/// sorted, deduplicated id list with binary-search membership tests, which is
/// compact and cache-friendly for the allow-set sizes the planner routes
/// through this path.
#[derive(Debug, Clone, Default)]
pub struct AllowSet {
    ids: Vec<u64>,
}

impl AllowSet {
    /// Build from an arbitrary id list; sorts and deduplicates.
    pub fn from_unsorted(mut ids: Vec<u64>) -> Self {
        ids.sort_unstable();
        ids.dedup();
        Self { ids }
    }

    /// Build from an already sorted, duplicate-free id list.
    pub fn from_sorted_unique(ids: Vec<u64>) -> Self {
        debug_assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids must be sorted and unique");
        Self { ids }
    }

    pub fn contains(&self, doc: u64) -> bool {
        self.ids.binary_search(&doc).is_ok()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.ids
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.ids.iter().copied()
    }

    /// Fraction of the corpus this set covers, in `[0, 1]`.
    pub fn selectivity(&self, total_docs: usize) -> f64 {
        if total_docs == 0 {
            0.0
        } else {
            self.ids.len() as f64 / total_docs as f64
        }
    }
}

/// Statistics about an ANN (IVF) index available for a vector field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnIndexStats {
    /// Number of IVF lists (centroids) in the index.
    pub n_lists: usize,
    /// Number of documents covered by the index.
    pub indexed_docs: usize,
}

/// One queryable view of a namespace.
///
/// Implementations are expected to be cheap to call repeatedly; the serving
/// layer implements this on top of cached segments, and tests implement it
/// over in-memory tables.
///
/// All search methods must return hits sorted by descending score (ties
/// broken by ascending doc id) and respect the `allow` set when provided.
pub trait QuerySource {
    /// The filter expression type this source can evaluate. The executor
    /// treats filters as opaque and only passes them through.
    type Filter;
    /// The materialized (projected) document type returned to callers.
    type Doc;

    /// Total number of live documents in this view.
    fn total_docs(&self) -> usize;

    /// ANN index statistics for a vector field, if an IVF index exists.
    fn ann_stats(&self, field: &str) -> Option<AnnIndexStats>;

    /// Whether a persistent full-text (inverted) index is available.
    fn has_text_index(&self) -> bool;

    /// Cheap estimate of the fraction of documents matching `filter`,
    /// in `[0, 1]`, derived from attribute-index statistics. `None` when no
    /// estimate is available; the planner then defaults to pre-filtering.
    fn filter_selectivity_hint(&self, filter: &Self::Filter) -> Option<f64>;

    /// Fully evaluate `filter` against the attribute/filter indexes.
    fn eval_filter(&self, filter: &Self::Filter) -> Result<AllowSet, ExecError>;

    /// Test a single document against `filter` (used by the post-filter
    /// strategy, where materializing the full allow-set would be wasteful).
    fn matches_filter(&self, filter: &Self::Filter, doc: u64) -> Result<bool, ExecError>;

    /// Exact (brute-force) kNN over the (optionally restricted) corpus.
    fn exact_knn(
        &self,
        query: &VectorQuery,
        allow: Option<&AllowSet>,
        k: usize,
    ) -> Result<Vec<Hit>, ExecError>;

    /// IVF ANN search probing `nprobe` lists.
    fn ann_search(
        &self,
        query: &VectorQuery,
        allow: Option<&AllowSet>,
        k: usize,
        nprobe: usize,
    ) -> Result<Vec<Hit>, ExecError>;

    /// BM25 full-text search with per-field boosting.
    fn text_search(
        &self,
        query: &TextQuery,
        allow: Option<&AllowSet>,
        k: usize,
    ) -> Result<Vec<Hit>, ExecError>;

    /// Sparse-vector dot-product search.
    fn sparse_search(
        &self,
        query: &SparseQuery,
        allow: Option<&AllowSet>,
        k: usize,
    ) -> Result<Vec<Hit>, ExecError>;

    /// List documents matching the allow-set (or all documents when `None`)
    /// without any ranking signal, up to `limit`. Used for filter-only and
    /// match-all queries; hits should be ordered by ascending doc id with a
    /// constant score (conventionally `1.0`).
    fn scan_filter_only(
        &self,
        allow: Option<&AllowSet>,
        limit: usize,
    ) -> Result<Vec<Hit>, ExecError>;

    /// Materialize the final hits into projected documents. Must return
    /// exactly one document per hit, in the same order.
    fn materialize(
        &self,
        hits: &[Hit],
        projection: &Projection,
    ) -> Result<Vec<Self::Doc>, ExecError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_set_dedups_and_sorts() {
        let s = AllowSet::from_unsorted(vec![5, 1, 3, 3, 1]);
        assert_eq!(s.as_slice(), &[1, 3, 5]);
        assert_eq!(s.len(), 3);
        assert!(s.contains(3));
        assert!(!s.contains(2));
        assert!(!s.is_empty());
    }

    #[test]
    fn allow_set_selectivity() {
        let s = AllowSet::from_unsorted(vec![1, 2, 3, 4, 5]);
        assert!((s.selectivity(10) - 0.5).abs() < 1e-12);
        assert_eq!(AllowSet::default().selectivity(0), 0.0);
    }

    #[test]
    fn exec_error_display() {
        let e = ExecError::InvalidRequest("top_k must be at least 1".into());
        assert!(e.to_string().contains("invalid query request"));
        let e = ExecError::Source("io".into());
        assert!(e.to_string().contains("query source error"));
    }
}
