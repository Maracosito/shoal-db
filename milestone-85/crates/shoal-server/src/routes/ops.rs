//! Operational endpoints: cache warming, pinning, compaction, repair.

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Identity, Role};
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

use super::{authorize, ns_ref, record_audit};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmRequest {
    /// Specific segment IDs to warm. Omit to warm the whole namespace
    /// (manifest, filter indexes, IVF centroids, postings, segments).
    #[serde(default)]
    pub segments: Option<Vec<String>>,
    /// Also pin the namespace after warming so eviction skips it.
    #[serde(default)]
    pub pin: bool,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/warm`
///
/// Pulls the namespace's hot set from object storage into the local disk
/// cache and its metadata/index headers into the memory cache, so the next
/// query is served warm. Returns a report of segments fetched and bytes
/// loaded per tier. The body is optional; an empty POST warms everything.
pub async fn warm(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    body: Option<Json<WarmRequest>>,
) -> Result<Json<Value>, ApiError> {
    // Warming is a read-side optimisation, so readers may trigger it…
    authorize(&identity, &org, &project, Role::Reader)?;
    let opts = body.map(|Json(b)| b).unwrap_or_default();
    // …but pinning reserves cache capacity, which requires the writer role.
    if opts.pin {
        authorize(&identity, &org, &project, Role::Writer)?;
    }

    let nref = ns_ref(&org, &project, &ns);
    let report = state.engine.warm(&nref, opts.segments).await?;
    if opts.pin {
        state.engine.pin(&nref).await?;
    }

    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.warm",
        &org,
        &project,
        Some(&ns),
        true,
        200,
        Some(json!({ "pinned": opts.pin })),
    );

    Ok(Json(json!({ "warmed": report, "pinned": opts.pin })))
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/pin`
///
/// Pins the namespace: its cached segments are exempt from LRU eviction
/// until unpinned. Pin state is recorded in the namespace metadata so it
/// survives a node restart (the node re-warms pinned namespaces on boot).
pub async fn pin(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    let result = state.engine.pin(&ns_ref(&org, &project, &ns)).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.pin",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 404 },
        None,
    );
    result?;
    Ok(Json(json!({ "pinned": true })))
}

/// `DELETE /v1/orgs/:org/projects/:project/namespaces/:ns/pin`
pub async fn unpin(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    let result = state.engine.unpin(&ns_ref(&org, &project, &ns)).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.unpin",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 404 },
        None,
    );
    result?;
    Ok(Json(json!({ "pinned": false })))
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/compact`
///
/// Triggers a compaction round: outstanding WAL entries and small segments
/// are merged into larger immutable segments, indexes are rebuilt for the
/// merged data, and the manifest is atomically advanced. Returns the
/// compaction report (segments before/after, bytes rewritten, WAL drained).
pub async fn compact(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    let result = state.engine.compact(&ns_ref(&org, &project, &ns)).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.compact",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 409 },
        None,
    );
    let report = result?;
    Ok(Json(json!({ "compaction": report })))
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/repair`
///
/// Validates the namespace's manifest against object storage, removes
/// references to incomplete uploads, rebuilds missing index files from
/// surviving segments + WAL, and reconciles reference counts. Admin-only.
pub async fn repair(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Admin)?;
    let result = state.engine.repair(&ns_ref(&org, &project, &ns)).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.repair",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 409 },
        None,
    );
    let report = result?;
    Ok(Json(json!({ "repair": report })))
}
