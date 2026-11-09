//! Plan execution.
//!
//! Runs a [`QueryPlan`] against a [`QuerySource`]: evaluates the filter
//! strategy, executes each retrieval leg, fuses multi-leg results, truncates
//! to `top_k`, and materializes the attribute projection. Also performs one
//! adaptive refinement the planner cannot make from statistics alone: when a
//! pre-filter allow-set turns out to be small, an IVF leg is downgraded to
//! exact kNN over the allowed subset (cheaper and exact).

use super::fuse::{rrf_fuse, sort_hits_desc, weighted_fuse};
use super::planner::{plan_query, FilterStrategy, FusionPlan, LegKind, PlannerConfig, QueryPlan};
use super::request::{MultiQuery, QueryRequest};
use super::{AllowSet, ExecError, Hit, QuerySource};

/// Per-leg execution statistics.
#[derive(Debug, Clone)]
pub struct LegStats {
    pub kind: LegKind,
    pub requested_k: usize,
    pub returned: usize,
    /// True when an IVF leg was adaptively downgraded to exact search
    /// because the allow-set was small.
    pub adapted_exact: bool,
}

/// Execution statistics, surfaced for explain output and metrics.
#[derive(Debug, Clone, Default)]
pub struct ExecutionStats {
    pub used_pre_filter: bool,
    pub used_post_filter: bool,
    /// Size of the materialized allow-set when pre-filtering was used.
    pub allow_set_size: Option<usize>,
    pub legs: Vec<LegStats>,
    pub fused: bool,
    /// Total candidates produced across all legs (after post-filtering).
    pub total_candidates: usize,
}

/// One result row: fused score plus the projected document.
#[derive(Debug, Clone)]
pub struct Row<D> {
    pub score: f32,
    pub doc: D,
}

/// Output of executing one query.
#[derive(Debug, Clone)]
pub struct QueryOutput<D> {
    pub rows: Vec<Row<D>>,
    pub stats: ExecutionStats,
}

/// Plan and execute a query in one call.
pub fn execute<S: QuerySource + ?Sized>(
    req: &QueryRequest<S::Filter>,
    src: &S,
    cfg: &PlannerConfig,
) -> Result<QueryOutput<S::Doc>, ExecError> {
    let plan = plan_query(req, src, cfg)?;
    execute_plan(&plan, req, src, cfg)
}

/// Execute a batch of independent queries; results are returned per-query in
/// request order.
pub fn execute_multi<S: QuerySource + ?Sized>(
    multi: &MultiQuery<S::Filter>,
    src: &S,
    cfg: &PlannerConfig,
) -> Result<Vec<QueryOutput<S::Doc>>, ExecError> {
    multi.queries.iter().map(|q| execute(q, src, cfg)).collect()
}

/// Execute an already-built plan. Exposed separately so callers can inspect
/// or log the plan (explain) before running it, and for testing.
pub fn execute_plan<S: QuerySource + ?Sized>(
    plan: &QueryPlan,
    req: &QueryRequest<S::Filter>,
    src: &S,
    cfg: &PlannerConfig,
) -> Result<QueryOutput<S::Doc>, ExecError> {
    let mut stats = ExecutionStats::default();

    // -- filter -----------------------------------------------------------
    let allow: Option<AllowSet> = match plan.filter {
        FilterStrategy::PreFilter => {
            let f = req.filter.as_ref().ok_or_else(|| {
                ExecError::InvalidRequest("plan expects a filter but the request has none".into())
            })?;
            let set = src.eval_filter(f)?;
            stats.used_pre_filter = true;
            stats.allow_set_size = Some(set.len());
            if set.is_empty() {
                // Nothing can match; short-circuit.
                return Ok(QueryOutput {
                    rows: Vec::new(),
                    stats,
                });
            }
            Some(set)
        }
        FilterStrategy::PostFilter | FilterStrategy::None => None,
    };
    let post_filter = matches!(plan.filter, FilterStrategy::PostFilter);
    stats.used_post_filter = post_filter;

    // -- legs ---------------------------------------------------------------
    let mut leg_results: Vec<Vec<Hit>> = Vec::with_capacity(plan.legs.len());
    let mut weights: Vec<f32> = Vec::with_capacity(plan.legs.len());

    for leg in &plan.legs {
        let mut adapted = false;
        let mut hits = match leg.kind {
            LegKind::VectorExact => {
                let q = req.vector.as_ref().ok_or_else(|| {
                    ExecError::InvalidRequest("plan has a vector leg but request has no vector".into())
                })?;
                src.exact_knn(q, allow.as_ref(), leg.fetch_k)?
            }
            LegKind::VectorIvf { nprobe } => {
                let q = req.vector.as_ref().ok_or_else(|| {
                    ExecError::InvalidRequest("plan has a vector leg but request has no vector".into())
                })?;
                match allow.as_ref() {
                    Some(a) if a.len() <= cfg.small_allow_exact => {
                        // Adaptive downgrade: exact over the small allow-set.
                        adapted = true;
                        src.exact_knn(q, Some(a), leg.fetch_k)?
                    }
                    other => src.ann_search(q, other, leg.fetch_k, nprobe)?,
                }
            }
            LegKind::FullText => {
                let q = req.text.as_ref().ok_or_else(|| {
                    ExecError::InvalidRequest("plan has a text leg but request has no text query".into())
                })?;
                src.text_search(q, allow.as_ref(), leg.fetch_k)?
            }
            LegKind::Sparse => {
                let q = req.sparse.as_ref().ok_or_else(|| {
                    ExecError::InvalidRequest(
                        "plan has a sparse leg but request has no sparse query".into(),
                    )
                })?;
                src.sparse_search(q, allow.as_ref(), leg.fetch_k)?
            }
            LegKind::FilterScan => src.scan_filter_only(allow.as_ref(), leg.fetch_k)?,
        };

        if post_filter {
            let f = req.filter.as_ref().ok_or_else(|| {
                ExecError::InvalidRequest("plan expects a filter but the request has none".into())
            })?;
            let mut kept = Vec::with_capacity(hits.len());
            for h in hits {
                if src.matches_filter(f, h.doc)? {
                    kept.push(h);
                }
            }
            hits = kept;
        }

        stats.legs.push(LegStats {
            kind: leg.kind,
            requested_k: leg.fetch_k,
            returned: hits.len(),
            adapted_exact: adapted,
        });
        stats.total_candidates += hits.len();
        weights.push(leg.weight);
        leg_results.push(hits);
    }

    // -- fusion ---------------------------------------------------------------
    let mut fused: Vec<Hit> = match &plan.fusion {
        Some(FusionPlan::Rrf { k }) => {
            stats.fused = true;
            rrf_fuse(&leg_results, &weights, *k)
        }
        Some(FusionPlan::Weighted { normalize }) => {
            stats.fused = true;
            weighted_fuse(&leg_results, &weights, *normalize)
        }
        None => leg_results.pop().unwrap_or_default(),
    };

    // Enforce ordering regardless of source behavior, then truncate.
    sort_hits_desc(&mut fused);
    fused.truncate(plan.top_k);

    // -- projection -------------------------------------------------------------
    let docs = src.materialize(&fused, &req.projection)?;
    if docs.len() != fused.len() {
        return Err(ExecError::Source(format!(
            "materialize returned {} documents for {} hits",
            docs.len(),
            fused.len()
        )));
    }
    let rows = fused
        .iter()
        .zip(docs)
        .map(|(h, doc)| Row {
            score: h.score,
            doc,
        })
        .collect();

    Ok(QueryOutput { rows, stats })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::request::{
        FusionSpec, Metric, Projection, QueryRequest, SparseQuery, TextQuery, VectorQuery,
    };
    use crate::plan::{AllowSet, AnnIndexStats, ExecError, Hit, QuerySource};

    #[derive(Debug, Clone, PartialEq)]
    struct MiniDoc {
        id: u64,
        num: Option<i64>,
    }

    /// Test filter: matches rows whose numeric attribute is >= threshold.
    #[derive(Debug, Clone)]
    struct NumAtLeast(i64);

    /// Tiny in-memory source: (doc id, vector, numeric attr, text token).
    struct MiniSource {
        rows: Vec<(u64, Vec<f32>, i64, &'static str)>,
    }

    impl MiniSource {
        fn fixture() -> Self {
            Self {
                rows: vec![
                    (1, vec![1.0, 0.0], 10, "apple"),
                    (2, vec![0.9, 0.1], 20, "banana"),
                    (3, vec![0.0, 1.0], 30, "apple"),
                    (4, vec![0.5, 0.5], 40, "cherry"),
                    (5, vec![0.8, 0.2], 50, "apple"),
                ],
            }
        }

        fn row(&self, id: u64) -> Option<&(u64, Vec<f32>, i64, &'static str)> {
            self.rows.iter().find(|r| r.0 == id)
        }
    }

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    impl QuerySource for MiniSource {
        type Filter = NumAtLeast;
        type Doc = MiniDoc;

        fn total_docs(&self) -> usize {
            self.rows.len()
        }
        fn ann_stats(&self, _field: &str) -> Option<AnnIndexStats> {
            Some(AnnIndexStats {
                n_lists: 2,
                indexed_docs: self.rows.len(),
            })
        }
        fn has_text_index(&self) -> bool {
            true
        }
        fn filter_selectivity_hint(&self, _f: &NumAtLeast) -> Option<f64> {
            None
        }
        fn eval_filter(&self, f: &NumAtLeast) -> Result<AllowSet, ExecError> {
            Ok(AllowSet::from_unsorted(
                self.rows
                    .iter()
                    .filter(|r| r.2 >= f.0)
                    .map(|r| r.0)
                    .collect(),
            ))
        }
        fn matches_filter(&self, f: &NumAtLeast, doc: u64) -> Result<bool, ExecError> {
            Ok(self.row(doc).map(|r| r.2 >= f.0).unwrap_or(false))
        }
        fn exact_knn(
            &self,
            q: &VectorQuery,
            allow: Option<&AllowSet>,
            k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            let mut hits: Vec<Hit> = self
                .rows
                .iter()
                .filter(|r| allow.map(|a| a.contains(r.0)).unwrap_or(true))
                .map(|r| Hit {
                    doc: r.0,
                    score: dot(&q.vector, &r.1),
                })
                .collect();
            sort_hits_desc(&mut hits);
            hits.truncate(k);
            Ok(hits)
        }
        fn ann_search(
            &self,
            q: &VectorQuery,
            allow: Option<&AllowSet>,
            k: usize,
            _nprobe: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            self.exact_knn(q, allow, k)
        }
        fn text_search(
            &self,
            q: &TextQuery,
            allow: Option<&AllowSet>,
            k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            let mut hits: Vec<Hit> = self
                .rows
                .iter()
                .filter(|r| allow.map(|a| a.contains(r.0)).unwrap_or(true))
                .filter(|r| r.3 == q.query)
                .map(|r| Hit { doc: r.0, score: 1.0 })
                .collect();
            sort_hits_desc(&mut hits);
            hits.truncate(k);
            Ok(hits)
        }
        fn sparse_search(
            &self,
            _q: &SparseQuery,
            _allow: Option<&AllowSet>,
            _k: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            Err(ExecError::Source("no sparse field in fixture".into()))
        }
        fn scan_filter_only(
            &self,
            allow: Option<&AllowSet>,
            limit: usize,
        ) -> Result<Vec<Hit>, ExecError> {
            let mut ids: Vec<u64> = self
                .rows
                .iter()
                .filter(|r| allow.map(|a| a.contains(r.0)).unwrap_or(true))
                .map(|r| r.0)
                .collect();
            ids.sort_unstable();
            ids.truncate(limit);
            Ok(ids.into_iter().map(|doc| Hit { doc, score: 1.0 }).collect())
        }
        fn materialize(
            &self,
            hits: &[Hit],
            projection: &Projection,
        ) -> Result<Vec<MiniDoc>, ExecError> {
            hits.iter()
                .map(|h| {
                    let row = self
                        .row(h.doc)
                        .ok_or_else(|| ExecError::Source(format!("unknown doc {}", h.doc)))?;
                    let num = match projection {
                        Projection::All => Some(row.2),
                        Projection::Fields(fs) if fs.iter().any(|f| f == "num") => Some(row.2),
                        _ => None,
                    };
                    Ok(MiniDoc { id: h.doc, num })
                })
                .collect()
        }
    }

    fn vq() -> VectorQuery {
        VectorQuery {
            field: "embedding".into(),
            vector: vec![1.0, 0.0],
            metric: Metric::Dot,
        }
    }

    fn cfg() -> PlannerConfig {
        PlannerConfig::default()
    }

    fn ids<D>(out: &QueryOutput<D>, f: impl Fn(&D) -> u64) -> Vec<u64> {
        out.rows.iter().map(|r| f(&r.doc)).collect()
    }

    #[test]
    fn vector_query_returns_nearest() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(2).with_vector(vq());
        let out = execute(&req, &src, &cfg()).unwrap();
        assert_eq!(ids(&out, |d| d.id), vec![1, 2]);
        assert!(out.rows[0].score >= out.rows[1].score);
        assert!(!out.stats.used_pre_filter);
        assert!(!out.stats.fused);
    }

    #[test]
    fn pre_filter_restricts_candidates() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(2)
            .with_vector(vq())
            .with_filter(NumAtLeast(30));
        let out = execute(&req, &src, &cfg()).unwrap();
        // allowed: {3, 4, 5}; nearest to [1, 0]: 5 (0.8), 4 (0.5)
        assert_eq!(ids(&out, |d| d.id), vec![5, 4]);
        assert!(out.stats.used_pre_filter);
        assert_eq!(out.stats.allow_set_size, Some(3));
    }

    #[test]
    fn empty_allow_set_short_circuits() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(5)
            .with_vector(vq())
            .with_filter(NumAtLeast(100));
        let out = execute(&req, &src, &cfg()).unwrap();
        assert!(out.rows.is_empty());
        assert_eq!(out.stats.allow_set_size, Some(0));
        assert!(out.stats.legs.is_empty());
    }

    #[test]
    fn hybrid_rrf_combines_vector_and_text() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(3).with_vector(vq()).with_text(TextQuery {
            query: "apple".into(),
            fields: vec![],
        });
        let out = execute(&req, &src, &cfg()).unwrap();
        // vector ranking: 1, 2, 5, 4, 3; text matches (id order): 1, 3, 5.
        // RRF(k=60): doc1 = 1/61 + 1/61; doc5 = 1/63 + 1/63; doc3 = 1/65 + 1/62.
        assert_eq!(ids(&out, |d| d.id), vec![1, 5, 3]);
        assert!(out.stats.fused);
        assert_eq!(out.stats.legs.len(), 2);
    }

    #[test]
    fn weighted_fusion_end_to_end() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(5)
            .with_vector(vq())
            .with_text(TextQuery {
                query: "apple".into(),
                fields: vec![],
            })
            .with_fusion(FusionSpec::Weighted {
                vector: 0.0,
                text: 1.0,
                sparse: 0.0,
                normalize: true,
            });
        let out = execute(&req, &src, &cfg()).unwrap();
        // Vector leg weight 0 → ranking determined purely by text matches.
        assert_eq!(ids(&out, |d| d.id), vec![1, 3, 5]);
    }

    #[test]
    fn filter_only_query_lists_matches() {
        let src = MiniSource::fixture();
        let req: QueryRequest<NumAtLeast> = QueryRequest::new(10).with_filter(NumAtLeast(30));
        let out = execute(&req, &src, &cfg()).unwrap();
        assert_eq!(ids(&out, |d| d.id), vec![3, 4, 5]);
        for r in &out.rows {
            assert!((r.score - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn match_all_listing_respects_top_k() {
        let src = MiniSource::fixture();
        let req: QueryRequest<NumAtLeast> = QueryRequest::new(2);
        let out = execute(&req, &src, &cfg()).unwrap();
        assert_eq!(ids(&out, |d| d.id), vec![1, 2]);
    }

    #[test]
    fn projection_controls_returned_attributes() {
        let src = MiniSource::fixture();
        let req = QueryRequest::new(1)
            .with_vector(vq())
            .with_projection(Projection::Fields(vec!["num".into()]));
        let out = execute(&req, &src, &cfg()).unwrap();
        assert_eq!(out.rows[0].doc, MiniDoc { id: 1, num: Some(10) });

        let req = QueryRequest::new(1)
            .with_vector(vq())
            .with_projection(Projection::IdOnly);
        let out = execute(&req, &src, &cfg()).unwrap();
        assert_eq!(out.rows[0].doc, MiniDoc { id: 1, num: None });
    }

    #[test]
    fn post_filter_plan_drops_non_matching_hits() {
        let src = MiniSource::fixture();
        // Hand-built plan exercising the post-filter path directly.
        let plan = QueryPlan {
            filter: FilterStrategy::PostFilter,
            legs: vec![crate::plan::planner::PlanLeg {
                kind: LegKind::VectorExact,
                fetch_k: 5,
                weight: 1.0,
            }],
            fusion: None,
            top_k: 2,
            notes: vec![],
        };
        let req = QueryRequest::new(2)
            .with_vector(vq())
            .with_filter(NumAtLeast(30));
        let out = execute_plan(&plan, &req, &src, &cfg()).unwrap();
        // unrestricted ranking: 1, 2, 5, 4, 3 → post-filter keeps 5, 4, 3 → top 2.
        assert_eq!(ids(&out, |d| d.id), vec![5, 4]);
        assert!(out.stats.used_post_filter);
        assert!(!out.stats.used_pre_filter);
    }

    #[test]
    fn ivf_leg_adaptively_downgrades_to_exact_on_small_allow_set() {
        let src = MiniSource::fixture();
        let plan = QueryPlan {
            filter: FilterStrategy::PreFilter,
            legs: vec![crate::plan::planner::PlanLeg {
                kind: LegKind::VectorIvf { nprobe: 1 },
                fetch_k: 5,
                weight: 1.0,
            }],
            fusion: None,
            top_k: 3,
            notes: vec![],
        };
        let req = QueryRequest::new(3)
            .with_vector(vq())
            .with_filter(NumAtLeast(30));
        let out = execute_plan(&plan, &req, &src, &cfg()).unwrap();
        assert_eq!(ids(&out, |d| d.id), vec![5, 4, 3]);
        assert!(out.stats.legs[0].adapted_exact);
    }

    #[test]
    fn multi_query_returns_per_query_results() {
        let src = MiniSource::fixture();
        let multi = MultiQuery::new(vec![
            QueryRequest::new(1).with_vector(vq()),
            QueryRequest::new(10).with_filter(NumAtLeast(40)),
        ]);
        let outs = execute_multi(&multi, &src, &cfg()).unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(ids(&outs[0], |d| d.id), vec![1]);
        assert_eq!(ids(&outs[1], |d| d.id), vec![4, 5]);
    }
}
