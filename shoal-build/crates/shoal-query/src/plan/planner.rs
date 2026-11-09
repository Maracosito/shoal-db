//! The query planner.
//!
//! Pure function of (request, source statistics, config) → [`QueryPlan`].
//! The planner never touches data; it consumes only cheap statistics
//! (corpus size, index availability, filter selectivity hints) so it can run
//! before any segment is fetched from object storage. The executor performs
//! one *adaptive* refinement at runtime: if a pre-filter allow-set turns out
//! to be tiny, an IVF leg is downgraded to exact search over the allowed
//! subset (see `executor.rs`).

use super::request::{AnnMode, FusionSpec, QueryRequest, VectorQuery};
use super::{AnnIndexStats, ExecError, QuerySource};

/// Heuristic thresholds used by the planner. Defaults are intentionally
/// conservative; all of them are surfaced in server configuration.
#[derive(Debug, Clone)]
pub struct PlannerConfig {
    /// Below this corpus size, exact kNN is always chosen (the brute-force
    /// scan is cheaper than centroid routing plus posting-list fetches).
    pub exact_search_threshold: usize,
    /// If the (estimated or actual) allow-set is at most this many documents,
    /// exact search over the allowed subset beats probing IVF lists.
    pub small_allow_exact: usize,
    /// When a filter is estimated to match at least this fraction of the
    /// corpus, post-filtering (run the leg unrestricted, then drop
    /// non-matching hits) is cheaper than materializing the allow-set.
    pub post_filter_min_selectivity: f64,
    /// Multiplier applied to the heuristic `nprobe` when the query is
    /// filtered, to compensate for candidates lost to the filter.
    pub filtered_nprobe_boost: f64,
    /// Per-leg over-fetch factor when results will be fused across legs.
    pub fusion_oversample: f32,
    /// Per-leg over-fetch factor when post-filtering will drop candidates.
    pub post_filter_oversample: f32,
    /// Hard cap on per-leg fetch size after oversampling.
    pub max_fetch_k: usize,
    /// Hard cap on user-requested `top_k`.
    pub max_top_k: usize,
    /// RRF constant used when fusion is `Auto`.
    pub rrf_k: f32,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            exact_search_threshold: 2_000,
            small_allow_exact: 1_024,
            post_filter_min_selectivity: 0.9,
            filtered_nprobe_boost: 2.0,
            fusion_oversample: 3.0,
            post_filter_oversample: 4.0,
            max_fetch_k: 10_000,
            max_top_k: 1_000,
            rrf_k: 60.0,
        }
    }
}

/// How the metadata filter participates in execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStrategy {
    /// No filter in the request.
    None,
    /// Evaluate the filter against the attribute indexes up front and pass
    /// the allow-set into every leg.
    PreFilter,
    /// Run legs unrestricted and drop non-matching hits afterwards. Chosen
    /// only when the filter is estimated to match nearly everything.
    PostFilter,
}

/// The retrieval primitive a leg executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegKind {
    VectorExact,
    VectorIvf { nprobe: usize },
    FullText,
    Sparse,
    /// No ranking signal: list documents matching the filter (or everything).
    FilterScan,
}

/// One retrieval leg of the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanLeg {
    pub kind: LegKind,
    /// How many candidates this leg should produce (already oversampled for
    /// fusion / post-filtering).
    pub fetch_k: usize,
    /// Fusion weight for this leg.
    pub weight: f32,
}

/// Fusion stage of the plan (present iff there are at least two legs).
#[derive(Debug, Clone, PartialEq)]
pub enum FusionPlan {
    Rrf { k: f32 },
    Weighted { normalize: bool },
}

/// A fully resolved execution plan.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub filter: FilterStrategy,
    pub legs: Vec<PlanLeg>,
    pub fusion: Option<FusionPlan>,
    pub top_k: usize,
    /// Human-readable planning decisions, surfaced via explain/debug output.
    pub notes: Vec<String>,
}

/// Plan a single query against `src`.
pub fn plan_query<S: QuerySource + ?Sized>(
    req: &QueryRequest<S::Filter>,
    src: &S,
    cfg: &PlannerConfig,
) -> Result<QueryPlan, ExecError> {
    req.validate(cfg)?;

    let total = src.total_docs();
    let mut notes: Vec<String> = Vec::new();
    let mut legs: Vec<PlanLeg> = Vec::new();

    // Per-leg fusion weights (only meaningful when there are >= 2 legs).
    let (w_vec, w_text, w_sparse) = match &req.fusion {
        FusionSpec::Weighted {
            vector,
            text,
            sparse,
            ..
        } => (*vector, *text, *sparse),
        _ => (1.0, 1.0, 1.0),
    };

    let hint = req
        .filter
        .as_ref()
        .and_then(|f| src.filter_selectivity_hint(f));

    let ranked_signals = req.vector.is_some() as usize
        + req.text.is_some() as usize
        + req.sparse.is_some() as usize;

    // -- filter strategy -----------------------------------------------------
    let filter_strategy = if req.filter.is_none() {
        FilterStrategy::None
    } else if ranked_signals == 0 {
        // Filter-only queries always materialize the allow-set: it *is* the
        // result set.
        FilterStrategy::PreFilter
    } else if let Some(s) = hint {
        if s >= cfg.post_filter_min_selectivity {
            notes.push(format!(
                "filter selectivity ~{s:.3} >= {:.3}: post-filtering",
                cfg.post_filter_min_selectivity
            ));
            FilterStrategy::PostFilter
        } else {
            notes.push(format!("filter selectivity ~{s:.3}: pre-filtering via attribute index"));
            FilterStrategy::PreFilter
        }
    } else {
        notes.push("no selectivity estimate: defaulting to pre-filter".into());
        FilterStrategy::PreFilter
    };

    let post = matches!(filter_strategy, FilterStrategy::PostFilter);
    let fetch = fetch_k(req.top_k, ranked_signals, post, cfg);

    // -- ranking legs ---------------------------------------------------------
    if let Some(vq) = &req.vector {
        let stats = src.ann_stats(&vq.field);
        let kind = choose_vector_strategy(vq, req, stats, total, hint, cfg, &mut notes)?;
        legs.push(PlanLeg {
            kind,
            fetch_k: fetch,
            weight: w_vec,
        });
    }
    if req.text.is_some() {
        if !src.has_text_index() {
            return Err(ExecError::InvalidRequest(
                "request contains a full-text query but the namespace has no full-text index"
                    .into(),
            ));
        }
        legs.push(PlanLeg {
            kind: LegKind::FullText,
            fetch_k: fetch,
            weight: w_text,
        });
    }
    if req.sparse.is_some() {
        notes.push("sparse leg: brute-force dot-product over sparse postings".into());
        legs.push(PlanLeg {
            kind: LegKind::Sparse,
            fetch_k: fetch,
            weight: w_sparse,
        });
    }
    if legs.is_empty() {
        notes.push(if req.filter.is_some() {
            "no ranking signal: filter-only scan".into()
        } else {
            "no ranking signal and no filter: match-all listing".into()
        });
        legs.push(PlanLeg {
            kind: LegKind::FilterScan,
            fetch_k: req.top_k,
            weight: 1.0,
        });
    }

    // -- fusion ----------------------------------------------------------------
    let fusion = if legs.len() >= 2 {
        Some(match &req.fusion {
            FusionSpec::Auto => FusionPlan::Rrf { k: cfg.rrf_k },
            FusionSpec::Rrf { k } => FusionPlan::Rrf { k: *k },
            FusionSpec::Weighted { normalize, .. } => FusionPlan::Weighted {
                normalize: *normalize,
            },
        })
    } else {
        None
    };

    Ok(QueryPlan {
        filter: filter_strategy,
        legs,
        fusion,
        top_k: req.top_k,
        notes,
    })
}

fn choose_vector_strategy<F>(
    vq: &VectorQuery,
    req: &QueryRequest<F>,
    stats: Option<AnnIndexStats>,
    total: usize,
    hint: Option<f64>,
    cfg: &PlannerConfig,
    notes: &mut Vec<String>,
) -> Result<LegKind, ExecError> {
    let filtered = req.filter.is_some();
    match req.ann.mode {
        AnnMode::ForceExact => {
            notes.push("vector leg: exact kNN (forced)".into());
            return Ok(LegKind::VectorExact);
        }
        AnnMode::ForceAnn => {
            let st = stats.ok_or_else(|| {
                ExecError::InvalidRequest(format!(
                    "ANN search forced but field '{}' has no ANN index",
                    vq.field
                ))
            })?;
            let nprobe = pick_nprobe(&st, req.ann.nprobe, filtered, cfg);
            notes.push(format!("vector leg: IVF nprobe={nprobe} (forced)"));
            return Ok(LegKind::VectorIvf { nprobe });
        }
        AnnMode::Auto => {}
    }

    let st = match stats {
        Some(st) => st,
        None => {
            notes.push(format!(
                "vector leg: exact kNN (no ANN index for field '{}')",
                vq.field
            ));
            return Ok(LegKind::VectorExact);
        }
    };

    if total <= cfg.exact_search_threshold {
        notes.push(format!(
            "vector leg: exact kNN (corpus size {total} <= {})",
            cfg.exact_search_threshold
        ));
        return Ok(LegKind::VectorExact);
    }

    if filtered {
        if let Some(s) = hint {
            let expected = (s * total as f64) as usize;
            if expected <= cfg.small_allow_exact {
                notes.push(format!(
                    "vector leg: exact kNN over allow-set (~{expected} docs <= {})",
                    cfg.small_allow_exact
                ));
                return Ok(LegKind::VectorExact);
            }
        }
    }

    let nprobe = pick_nprobe(&st, req.ann.nprobe, filtered, cfg);
    notes.push(format!(
        "vector leg: IVF nprobe={nprobe} of {} lists",
        st.n_lists
    ));
    Ok(LegKind::VectorIvf { nprobe })
}

fn pick_nprobe(
    st: &AnnIndexStats,
    override_nprobe: Option<usize>,
    filtered: bool,
    cfg: &PlannerConfig,
) -> usize {
    let max = st.n_lists.max(1);
    if let Some(n) = override_nprobe {
        return n.clamp(1, max);
    }
    let mut np = (st.n_lists as f64).sqrt().ceil();
    if filtered {
        np *= cfg.filtered_nprobe_boost;
    }
    (np as usize).clamp(1, max)
}

fn fetch_k(top_k: usize, ranked_legs: usize, post_filter: bool, cfg: &PlannerConfig) -> usize {
    let mut k = top_k as f32;
    if ranked_legs >= 2 {
        k *= cfg.fusion_oversample;
    }
    if post_filter {
        k *= cfg.post_filter_oversample;
    }
    (k.ceil() as usize).clamp(top_k, cfg.max_fetch_k.max(top_k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::request::{
        AnnMode, AnnOptions, FieldBoost, FusionSpec, Metric, Projection, QueryRequest, SparseQuery,
        TextQuery, VectorQuery,
    };
    use crate::plan::{AllowSet, AnnIndexStats, ExecError, Hit, QuerySource};

    /// Statistics-only stub. Search methods are never reached by the planner.
    struct Stub {
        total: usize,
        ann: Option<AnnIndexStats>,
        text: bool,
        hint: Option<f64>,
    }

    impl QuerySource for Stub {
        type Filter = ();
        type Doc = ();

        fn total_docs(&self) -> usize {
            self.total
        }
        fn ann_stats(&self, _field: &str) -> Option<AnnIndexStats> {
            self.ann
        }
        fn has_text_index(&self) -> bool {
            self.text
        }
        fn filter_selectivity_hint(&self, _f: &()) -> Option<f64> {
            self.hint
        }
        fn eval_filter(&self, _f: &()) -> Result<AllowSet, ExecError> {
            Ok(AllowSet::from_unsorted(vec![1, 2, 3]))
        }
        fn matches_filter(&self, _f: &(), _doc: u64) -> Result<bool, ExecError> {
            Ok(true)
        }
        fn exact_knn(
            &self,
            _q: &VectorQuery,
            _a: Option<&AllowSet>,
            _k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
        fn ann_search(
            &self,
            _q: &VectorQuery,
            _a: Option<&AllowSet>,
            _k: usize,
            _nprobe: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
        fn text_search(
            &self,
            _q: &TextQuery,
            _a: Option<&AllowSet>,
            _k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
        fn sparse_search(
            &self,
            _q: &SparseQuery,
            _a: Option<&AllowSet>,
            _k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
        fn scan_filter_only(
            &self,
            _a: Option<&AllowSet>,
            _l: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
        fn materialize(
            &self,
            _h: &[Hit],
            _p: &Projection,
        ) -> Result<Vec<()>, ExecError> {
            Err(ExecError::Source("stub".into()))
        }
    }

    fn big_indexed(hint: Option<f64>) -> Stub {
        Stub {
            total: 100_000,
            ann: Some(AnnIndexStats {
                n_lists: 100,
                indexed_docs: 100_000,
            }),
            text: true,
            hint,
        }
    }

    fn vq() -> VectorQuery {
        VectorQuery {
            field: "embedding".into(),
            vector: vec![1.0, 0.0],
            metric: Metric::Cosine,
        }
    }

    fn tq() -> TextQuery {
        TextQuery {
            query: "hello".into(),
            fields: vec![FieldBoost {
                field: "body".into(),
                boost: 1.0,
            }],
        }
    }

    fn cfg() -> PlannerConfig {
        PlannerConfig::default()
    }

    #[test]
    fn small_corpus_uses_exact() {
        let src = Stub {
            total: 500,
            ann: Some(AnnIndexStats {
                n_lists: 16,
                indexed_docs: 500,
            }),
            text: false,
            hint: None,
        };
        let plan = plan_query(&QueryRequest::new(10).with_vector(vq()), &src, &cfg()).unwrap();
        assert_eq!(plan.legs.len(), 1);
        assert_eq!(plan.legs[0].kind, LegKind::VectorExact);
        assert_eq!(plan.filter, FilterStrategy::None);
        assert!(plan.fusion.is_none());
        assert_eq!(plan.top_k, 10);
    }

    #[test]
    fn missing_index_falls_back_to_exact() {
        let src = Stub {
            total: 100_000,
            ann: None,
            text: false,
            hint: None,
        };
        let plan = plan_query(&QueryRequest::new(10).with_vector(vq()), &src, &cfg()).unwrap();
        assert_eq!(plan.legs[0].kind, LegKind::VectorExact);
    }

    #[test]
    fn large_corpus_uses_ivf_with_sqrt_nprobe() {
        let src = big_indexed(None);
        let plan = plan_query(&QueryRequest::new(10).with_vector(vq()), &src, &cfg()).unwrap();
        // sqrt(100) = 10
        assert_eq!(plan.legs[0].kind, LegKind::VectorIvf { nprobe: 10 });
        assert_eq!(plan.legs[0].fetch_k, 10); // single leg, no oversample
    }

    #[test]
    fn filter_boosts_nprobe_and_pre_filters() {
        let src = big_indexed(Some(0.5));
        let plan = plan_query(
            &QueryRequest::new(10).with_vector(vq()).with_filter(()),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.filter, FilterStrategy::PreFilter);
        // sqrt(100) * 2.0 boost = 20
        assert_eq!(plan.legs[0].kind, LegKind::VectorIvf { nprobe: 20 });
    }

    #[test]
    fn nprobe_override_is_respected_and_clamped() {
        let src = big_indexed(None);
        let req = QueryRequest::new(10).with_vector(vq()).with_ann(AnnOptions {
            mode: AnnMode::Auto,
            nprobe: Some(37),
        });
        let plan = plan_query(&req, &src, &cfg()).unwrap();
        assert_eq!(plan.legs[0].kind, LegKind::VectorIvf { nprobe: 37 });

        let req = QueryRequest::new(10).with_vector(vq()).with_ann(AnnOptions {
            mode: AnnMode::Auto,
            nprobe: Some(10_000),
        });
        let plan = plan_query(&req, &src, &cfg()).unwrap();
        assert_eq!(plan.legs[0].kind, LegKind::VectorIvf { nprobe: 100 });
    }

    #[test]
    fn force_exact_overrides_index() {
        let src = big_indexed(None);
        let req = QueryRequest::new(10).with_vector(vq()).with_ann(AnnOptions {
            mode: AnnMode::ForceExact,
            nprobe: None,
        });
        let plan = plan_query(&req, &src, &cfg()).unwrap();
        assert_eq!(plan.legs[0].kind, LegKind::VectorExact);
    }

    #[test]
    fn force_ann_without_index_errors() {
        let src = Stub {
            total: 100_000,
            ann: None,
            text: false,
            hint: None,
        };
        let req = QueryRequest::new(10).with_vector(vq()).with_ann(AnnOptions {
            mode: AnnMode::ForceAnn,
            nprobe: None,
        });
        assert!(matches!(
            plan_query(&req, &src, &cfg()),
            Err(ExecError::InvalidRequest(_))
        ));
    }

    #[test]
    fn hybrid_two_legs_rrf_and_oversample() {
        let src = big_indexed(None);
        let plan = plan_query(
            &QueryRequest::new(10).with_vector(vq()).with_text(tq()),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.legs.len(), 2);
        assert_eq!(plan.fusion, Some(FusionPlan::Rrf { k: 60.0 }));
        // top_k 10 * fusion_oversample 3.0 = 30
        assert_eq!(plan.legs[0].fetch_k, 30);
        assert_eq!(plan.legs[1].fetch_k, 30);
        assert_eq!(plan.legs[1].kind, LegKind::FullText);
    }

    #[test]
    fn weighted_fusion_carries_weights_into_legs() {
        let src = big_indexed(None);
        let plan = plan_query(
            &QueryRequest::new(10)
                .with_vector(vq())
                .with_text(tq())
                .with_fusion(FusionSpec::Weighted {
                    vector: 0.7,
                    text: 0.3,
                    sparse: 0.0,
                    normalize: true,
                }),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.fusion, Some(FusionPlan::Weighted { normalize: true }));
        assert!((plan.legs[0].weight - 0.7).abs() < 1e-6);
        assert!((plan.legs[1].weight - 0.3).abs() < 1e-6);
    }

    #[test]
    fn high_selectivity_picks_post_filter_with_oversample() {
        let src = big_indexed(Some(0.95));
        let plan = plan_query(
            &QueryRequest::new(10).with_vector(vq()).with_filter(()),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.filter, FilterStrategy::PostFilter);
        // single leg, post-filter oversample 4.0: 10 * 4 = 40
        assert_eq!(plan.legs[0].fetch_k, 40);
    }

    #[test]
    fn tiny_expected_allowset_picks_exact() {
        let src = big_indexed(Some(0.001)); // ~100 expected matches
        let plan = plan_query(
            &QueryRequest::new(10).with_vector(vq()).with_filter(()),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.filter, FilterStrategy::PreFilter);
        assert_eq!(plan.legs[0].kind, LegKind::VectorExact);
    }

    #[test]
    fn filter_only_query_plans_a_scan() {
        let src = big_indexed(Some(0.5));
        let plan = plan_query(&QueryRequest::new(25).with_filter(()), &src, &cfg()).unwrap();
        assert_eq!(plan.filter, FilterStrategy::PreFilter);
        assert_eq!(plan.legs.len(), 1);
        assert_eq!(plan.legs[0].kind, LegKind::FilterScan);
        assert_eq!(plan.legs[0].fetch_k, 25);
        assert!(plan.fusion.is_none());
    }

    #[test]
    fn match_all_query_plans_a_scan_without_filter() {
        let src = big_indexed(None);
        let plan = plan_query(&QueryRequest::<()>::new(5), &src, &cfg()).unwrap();
        assert_eq!(plan.filter, FilterStrategy::None);
        assert_eq!(plan.legs[0].kind, LegKind::FilterScan);
    }

    #[test]
    fn text_query_without_index_errors() {
        let src = Stub {
            total: 100_000,
            ann: None,
            text: false,
            hint: None,
        };
        let req: QueryRequest<()> = QueryRequest::new(10).with_text(tq());
        assert!(matches!(
            plan_query(&req, &src, &cfg()),
            Err(ExecError::InvalidRequest(_))
        ));
    }

    #[test]
    fn sparse_leg_is_included_in_hybrid() {
        let src = big_indexed(None);
        let plan = plan_query(
            &QueryRequest::new(10)
                .with_vector(vq())
                .with_sparse(SparseQuery {
                    field: "sparse".into(),
                    indices: vec![1, 7],
                    values: vec![0.5, 0.25],
                }),
            &src,
            &cfg(),
        )
        .unwrap();
        assert_eq!(plan.legs.len(), 2);
        assert_eq!(plan.legs[1].kind, LegKind::Sparse);
        assert!(plan.fusion.is_some());
    }

    #[test]
    fn fetch_k_is_capped() {
        let mut c = cfg();
        c.max_fetch_k = 50;
        let src = big_indexed(Some(0.95));
        let plan = plan_query(
            &QueryRequest::new(40)
                .with_vector(vq())
                .with_text(tq())
                .with_filter(()),
            &src,
            &c,
        )
        .unwrap();
        // 40 * 3 * 4 = 480 would exceed the cap of 50.
        assert_eq!(plan.legs[0].fetch_k, 50);
    }
}
