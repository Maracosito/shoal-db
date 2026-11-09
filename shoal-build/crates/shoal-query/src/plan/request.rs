//! Request-level types consumed by the planner and executor.
//!
//! These are the *execution layer* request types. The HTTP wire format
//! (`crate::wire`) is translated into a `QueryRequest` by the API server;
//! keeping the two separate lets the wire format evolve independently of the
//! engine and keeps this layer free of any serialization concerns.

use super::planner::PlannerConfig;
use super::ExecError;

/// Distance metric for dense-vector legs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    Dot,
    Euclidean,
}

/// A dense-vector ranking signal.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorQuery {
    /// Name of the vector field/attribute to search.
    pub field: String,
    pub vector: Vec<f32>,
    pub metric: Metric,
}

/// Per-field boost for full-text queries.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldBoost {
    pub field: String,
    pub boost: f32,
}

/// A full-text (BM25) ranking signal.
#[derive(Debug, Clone, PartialEq)]
pub struct TextQuery {
    /// Raw query string; tokenized by the source's analyzer.
    pub query: String,
    /// Fields to search with their boosts. Empty means "all indexed text
    /// fields with boost 1.0".
    pub fields: Vec<FieldBoost>,
}

/// A sparse-vector ranking signal (e.g. SPLADE/BM25-style learned sparse).
#[derive(Debug, Clone, PartialEq)]
pub struct SparseQuery {
    pub field: String,
    /// Parallel arrays: dimension indices and their weights.
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

/// Selected-attribute projection applied when materializing results.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Projection {
    /// Return all stored attributes.
    #[default]
    All,
    /// Return only ids and scores.
    IdOnly,
    /// Return only the named attributes.
    Fields(Vec<String>),
}

/// How multiple ranking legs should be fused.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum FusionSpec {
    /// Let the planner choose (currently RRF with the configured constant).
    #[default]
    Auto,
    /// Reciprocal rank fusion: `score = Σ w_leg / (k + rank)`.
    Rrf { k: f32 },
    /// Weighted score fusion with optional per-leg min-max normalization.
    Weighted {
        vector: f32,
        text: f32,
        sparse: f32,
        normalize: bool,
    },
}

/// ANN execution mode for the vector leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnnMode {
    /// Planner decides based on corpus size, index availability, and filter
    /// selectivity.
    #[default]
    Auto,
    /// Always brute-force exact kNN.
    ForceExact,
    /// Always use the IVF index; error if none exists.
    ForceAnn,
}

/// Tuning knobs for the vector leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AnnOptions {
    pub mode: AnnMode,
    /// Explicit number of IVF lists to probe; overrides the planner's
    /// heuristic when set. Clamped to `[1, n_lists]`.
    pub nprobe: Option<usize>,
}

/// A single query against one namespace view.
///
/// Generic over the source's filter expression type `F`; the executor never
/// inspects filters, it only routes them to the source.
#[derive(Debug, Clone)]
pub struct QueryRequest<F> {
    pub vector: Option<VectorQuery>,
    pub text: Option<TextQuery>,
    pub sparse: Option<SparseQuery>,
    pub filter: Option<F>,
    pub top_k: usize,
    pub projection: Projection,
    pub fusion: FusionSpec,
    pub ann: AnnOptions,
}

impl<F> Default for QueryRequest<F> {
    fn default() -> Self {
        Self {
            vector: None,
            text: None,
            sparse: None,
            filter: None,
            top_k: 10,
            projection: Projection::All,
            fusion: FusionSpec::Auto,
            ann: AnnOptions::default(),
        }
    }
}

impl<F> QueryRequest<F> {
    pub fn new(top_k: usize) -> Self {
        Self {
            top_k,
            ..Self::default()
        }
    }

    pub fn with_vector(mut self, q: VectorQuery) -> Self {
        self.vector = Some(q);
        self
    }

    pub fn with_text(mut self, q: TextQuery) -> Self {
        self.text = Some(q);
        self
    }

    pub fn with_sparse(mut self, q: SparseQuery) -> Self {
        self.sparse = Some(q);
        self
    }

    pub fn with_filter(mut self, f: F) -> Self {
        self.filter = Some(f);
        self
    }

    pub fn with_projection(mut self, p: Projection) -> Self {
        self.projection = p;
        self
    }

    pub fn with_fusion(mut self, f: FusionSpec) -> Self {
        self.fusion = f;
        self
    }

    pub fn with_ann(mut self, a: AnnOptions) -> Self {
        self.ann = a;
        self
    }

    /// Validate the request against engine limits before planning.
    pub fn validate(&self, cfg: &PlannerConfig) -> Result<(), ExecError> {
        if self.top_k == 0 {
            return Err(ExecError::InvalidRequest("top_k must be at least 1".into()));
        }
        if self.top_k > cfg.max_top_k {
            return Err(ExecError::InvalidRequest(format!(
                "top_k {} exceeds the configured maximum {}",
                self.top_k, cfg.max_top_k
            )));
        }
        if let Some(v) = &self.vector {
            if v.field.is_empty() {
                return Err(ExecError::InvalidRequest(
                    "vector query must name a non-empty field".into(),
                ));
            }
            if v.vector.is_empty() {
                return Err(ExecError::InvalidRequest("query vector must not be empty".into()));
            }
            if v.vector.iter().any(|x| !x.is_finite()) {
                return Err(ExecError::InvalidRequest(
                    "query vector contains non-finite values".into(),
                ));
            }
        }
        if let Some(t) = &self.text {
            if t.query.trim().is_empty() {
                return Err(ExecError::InvalidRequest("text query must not be empty".into()));
            }
            for b in &t.fields {
                if b.field.is_empty() {
                    return Err(ExecError::InvalidRequest(
                        "text field boost must name a non-empty field".into(),
                    ));
                }
                if !(b.boost.is_finite() && b.boost > 0.0) {
                    return Err(ExecError::InvalidRequest(format!(
                        "boost for field '{}' must be a positive finite number",
                        b.field
                    )));
                }
            }
        }
        if let Some(s) = &self.sparse {
            if s.indices.len() != s.values.len() {
                return Err(ExecError::InvalidRequest(format!(
                    "sparse query indices ({}) and values ({}) must have equal length",
                    s.indices.len(),
                    s.values.len()
                )));
            }
            if s.indices.is_empty() {
                return Err(ExecError::InvalidRequest("sparse query must not be empty".into()));
            }
            if s.values.iter().any(|x| !x.is_finite()) {
                return Err(ExecError::InvalidRequest(
                    "sparse query contains non-finite values".into(),
                ));
            }
        }
        match &self.fusion {
            FusionSpec::Rrf { k } => {
                if !(k.is_finite() && *k > 0.0) {
                    return Err(ExecError::InvalidRequest(
                        "rrf constant k must be a positive finite number".into(),
                    ));
                }
            }
            FusionSpec::Weighted {
                vector,
                text,
                sparse,
                ..
            } => {
                let ws = [*vector, *text, *sparse];
                if ws.iter().any(|w| !w.is_finite() || *w < 0.0) {
                    return Err(ExecError::InvalidRequest(
                        "fusion weights must be non-negative finite numbers".into(),
                    ));
                }
                if ws.iter().sum::<f32>() <= 0.0 {
                    return Err(ExecError::InvalidRequest(
                        "at least one fusion weight must be positive".into(),
                    ));
                }
            }
            FusionSpec::Auto => {}
        }
        Ok(())
    }
}

/// A batch of independent queries executed against the same namespace view.
/// Results are returned per-query in order.
#[derive(Debug, Clone, Default)]
pub struct MultiQuery<F> {
    pub queries: Vec<QueryRequest<F>>,
}

impl<F> MultiQuery<F> {
    pub fn new(queries: Vec<QueryRequest<F>>) -> Self {
        Self { queries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PlannerConfig {
        PlannerConfig::default()
    }

    fn vq() -> VectorQuery {
        VectorQuery {
            field: "embedding".into(),
            vector: vec![1.0, 0.0],
            metric: Metric::Cosine,
        }
    }

    #[test]
    fn rejects_zero_top_k() {
        let r: QueryRequest<()> = QueryRequest::new(0).with_vector(vq());
        assert!(matches!(r.validate(&cfg()), Err(ExecError::InvalidRequest(_))));
    }

    #[test]
    fn rejects_oversized_top_k() {
        let r: QueryRequest<()> = QueryRequest::new(cfg().max_top_k + 1).with_vector(vq());
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_empty_vector() {
        let r: QueryRequest<()> = QueryRequest::new(5).with_vector(VectorQuery {
            field: "embedding".into(),
            vector: vec![],
            metric: Metric::Dot,
        });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_nan_vector() {
        let r: QueryRequest<()> = QueryRequest::new(5).with_vector(VectorQuery {
            field: "embedding".into(),
            vector: vec![f32::NAN],
            metric: Metric::Dot,
        });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_blank_text_query() {
        let r: QueryRequest<()> = QueryRequest::new(5).with_text(TextQuery {
            query: "   ".into(),
            fields: vec![],
        });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_non_positive_boost() {
        let r: QueryRequest<()> = QueryRequest::new(5).with_text(TextQuery {
            query: "hello".into(),
            fields: vec![FieldBoost {
                field: "title".into(),
                boost: 0.0,
            }],
        });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_mismatched_sparse_arrays() {
        let r: QueryRequest<()> = QueryRequest::new(5).with_sparse(SparseQuery {
            field: "sparse".into(),
            indices: vec![1, 2],
            values: vec![0.5],
        });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn rejects_all_zero_fusion_weights() {
        let r: QueryRequest<()> = QueryRequest::new(5)
            .with_vector(vq())
            .with_fusion(FusionSpec::Weighted {
                vector: 0.0,
                text: 0.0,
                sparse: 0.0,
                normalize: true,
            });
        assert!(r.validate(&cfg()).is_err());
    }

    #[test]
    fn accepts_well_formed_request() {
        let r: QueryRequest<()> = QueryRequest::new(10)
            .with_vector(vq())
            .with_text(TextQuery {
                query: "hello world".into(),
                fields: vec![FieldBoost {
                    field: "title".into(),
                    boost: 2.0,
                }],
            })
            .with_fusion(FusionSpec::Rrf { k: 60.0 });
        assert!(r.validate(&cfg()).is_ok());
    }
}
