//! Copy-on-write namespace branching.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Identity, Role};
use crate::engine::NamespaceInfo;
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

use super::{authorize, ns_ref, record_audit, validate_name};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRequest {
    /// Name of the branch namespace to create within the same project.
    pub target: String,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/branch`
///
/// Creates a copy-on-write branch: the new namespace's manifest references
/// the source's current immutable segments (incrementing each segment's
/// reference count) and starts an empty WAL of its own. The operation is
/// O(manifest size), not O(data size) — no segment bytes are copied.
///
/// Isolation guarantees enforced by the engine and covered by tests:
/// * writes to the branch never mutate shared segments (they produce new
///   WAL entries and, after compaction, new segments owned by the branch);
/// * writes to the source likewise produce new source-owned files;
/// * deleting either side only deletes objects whose reference count
///   reaches zero, so shared history is never destroyed prematurely;
/// * branches can themselves be branched (multi-level), with reference
///   counts accumulating per segment.
pub async fn create(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<BranchRequest>,
) -> Result<(StatusCode, Json<NamespaceInfo>), ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    validate_name(&body.target)?;
    if body.target == ns {
        return Err(ApiError::bad_request(
            "branch target must differ from the source namespace",
        ));
    }

    let src = ns_ref(&org, &project, &ns);
    let dst = ns_ref(&org, &project, &body.target);
    let result = state.engine.branch_namespace(&src, &dst).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.branch",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 201 } else { 409 },
        Some(json!({ "target": body.target })),
    );

    Ok((StatusCode::CREATED, Json(result?)))
}

/// `GET /v1/orgs/:org/projects/:project/namespaces/:ns/branches`
///
/// Lists namespaces in this project that were branched (directly) from the
/// given namespace, including branches of branches reachable transitively
/// via their recorded parent references.
pub async fn list(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;
    let branches = state
        .engine
        .list_branches(&ns_ref(&org, &project, &ns))
        .await?;
    Ok(Json(json!({ "branches": branches })))
}
