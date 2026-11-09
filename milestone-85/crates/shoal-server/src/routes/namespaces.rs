//! Namespace CRUD handlers.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Identity, Role};
use crate::engine::{NamespaceConfig, NamespaceInfo, NamespaceUpdate};
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

use super::{authorize, ns_ref, record_audit, validate_name};

#[derive(Debug, Deserialize)]
pub struct CreateNamespaceRequest {
    pub name: String,
    #[serde(default)]
    pub config: NamespaceConfig,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces`
pub async fn create(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project)): Path<(String, String)>,
    Json(body): Json<CreateNamespaceRequest>,
) -> Result<(StatusCode, Json<NamespaceInfo>), ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    validate_name(&body.name)?;

    let nref = ns_ref(&org, &project, &body.name);
    let result = state.engine.create_namespace(&nref, body.config).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.create",
        &org,
        &project,
        Some(&body.name),
        success,
        if success { 201 } else { 409 },
        None,
    );

    let info = result?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// `GET /v1/orgs/:org/projects/:project/namespaces`
pub async fn list(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;
    let namespaces = state.engine.list_namespaces(&org, &project).await?;
    Ok(Json(json!({ "namespaces": namespaces })))
}

/// `GET /v1/orgs/:org/projects/:project/namespaces/:ns`
///
/// Returns namespace metadata plus live statistics (document count, segment
/// count, WAL size, indexing lag, branch parentage, pin status).
pub async fn describe(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<NamespaceInfo>, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;
    let info = state.engine.describe_namespace(&ns_ref(&org, &project, &ns)).await?;
    Ok(Json(info))
}

/// `PATCH /v1/orgs/:org/projects/:project/namespaces/:ns`
///
/// Partial update of mutable namespace configuration (e.g. distance metric
/// defaults, BM25 field weights, compaction thresholds). Immutable settings
/// such as vector dimensionality are rejected by the engine.
pub async fn update(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<NamespaceUpdate>,
) -> Result<Json<NamespaceInfo>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;

    let result = state
        .engine
        .update_namespace(&ns_ref(&org, &project, &ns), body)
        .await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.update",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 400 },
        None,
    );

    Ok(Json(result?))
}

/// `DELETE /v1/orgs/:org/projects/:project/namespaces/:ns`
///
/// Deletes the namespace. Segment files shared with branches (in either
/// direction) are reference-counted and only removed from object storage
/// once no manifest references them.
pub async fn remove(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;

    let result = state.engine.delete_namespace(&ns_ref(&org, &project, &ns)).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.delete",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 204 } else { 404 },
        None,
    );

    result?;
    Ok(StatusCode::NO_CONTENT)
}
