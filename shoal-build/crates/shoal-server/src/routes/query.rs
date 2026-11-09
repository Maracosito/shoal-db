//! Query handlers: single query and multi-query.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::{Identity, Role};
use crate::engine::query::{QueryRequest, QueryResponse};
use crate::error::ApiError;
use crate::AppState;

use super::{authorize, ns_ref};

/// Upper bound on sub-queries per multi-query request, to keep a single HTTP
/// call from monopolising the executor.
const MAX_MULTI_QUERIES: usize = 16;

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/query`
///
/// The request body is the engine's query DTO: vector and/or full-text
/// rank-by clauses, metadata filter tree, fusion settings (weighted or RRF),
/// `top_k`, and an optional attribute projection. The planner inside the
/// engine selects exact kNN, IVF ANN, BM25, filter-only or hybrid execution.
pub async fn query(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;
    let response = state.engine.query(&ns_ref(&org, &project, &ns), &body).await?;
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiQueryRequest {
    pub queries: Vec<QueryRequest>,
}

#[derive(Debug, Serialize)]
pub struct MultiQueryResponse {
    pub results: Vec<QueryResponse>,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/multi-query`
///
/// Executes up to [`MAX_MULTI_QUERIES`] queries concurrently against the
/// same namespace snapshot reference. Results are returned in request order.
pub async fn multi_query(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<MultiQueryRequest>,
) -> Result<Json<MultiQueryResponse>, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;

    if body.queries.is_empty() {
        return Err(ApiError::bad_request("`queries` must not be empty"));
    }
    if body.queries.len() > MAX_MULTI_QUERIES {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_MULTI_QUERIES} queries per multi-query request"
        )));
    }

    let nref = ns_ref(&org, &project, &ns);
    let futures = body.queries.into_iter().map(|q| {
        let engine = state.engine.clone();
        let nref = nref.clone();
        async move { engine.query(&nref, &q).await }
    });

    let results = futures::future::try_join_all(futures).await?;
    Ok(Json(MultiQueryResponse { results }))
}
