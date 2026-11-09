//! Namespace export (streaming NDJSON) and deep copy.

use std::sync::Arc;

use async_stream::try_stream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Extension, Json,
};
use bytes::Bytes;
use futures::Stream;
use serde::Deserialize;

use crate::auth::{Identity, Role};
use crate::engine::{Engine, NamespaceInfo, NsPath};
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

use super::{authorize, ns_ref, record_audit, validate_name};

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    /// Resume cursor from a previous (interrupted) export.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Documents fetched per page from the engine while streaming.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
}

fn default_page_size() -> usize {
    1000
}

/// Stream a namespace's documents as newline-delimited JSON, one document
/// per line, paging through the engine's stable export cursor.
fn export_stream(
    engine: Arc<Engine>,
    nref: crate::engine::NsPath,
    mut cursor: Option<String>,
    page_size: usize,
) -> impl Stream<Item = Result<Bytes, anyhow::Error>> {
    try_stream! {
        loop {
            let (docs, next) = engine.export_page(&nref, cursor.clone(), page_size).await?;
            for doc in docs {
                let mut line = serde_json::to_vec(&doc)?;
                line.push(b'\n');
                yield Bytes::from(line);
            }
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
}

/// `GET /v1/orgs/:org/projects/:project/namespaces/:ns/export`
///
/// Responds with `application/x-ndjson` and begins streaming immediately, so
/// arbitrarily large namespaces can be exported in constant memory. The
/// export reads from a consistent manifest snapshot taken at request time;
/// concurrent writes do not appear mid-stream.
pub async fn export(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Query(params): Query<ExportParams>,
) -> Result<Response, ApiError> {
    authorize(&identity, &org, &project, Role::Reader)?;

    let nref = ns_ref(&org, &project, &ns);
    // Resolve the namespace up front so a missing namespace is a clean 404
    // rather than an error in the middle of a 200 stream.
    state.engine.describe_namespace(&nref).await?;

    let page_size = params.page_size.clamp(1, 10_000);
    let stream = export_stream(state.engine.clone(), nref, params.cursor, page_size);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(stream))
        .expect("static response parts are valid"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyRequest {
    /// Name of the new namespace within the same project.
    pub target: String,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/copy`
///
/// Deep copy: the target namespace gets its *own* manifest and its own
/// reference counts on a fresh set of segment objects, so it shares no
/// lifecycle with the source. For an instant copy-on-write clone, use the
/// `branch` endpoint instead.
pub async fn copy(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<CopyRequest>,
) -> Result<(StatusCode, Json<NamespaceInfo>), ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    validate_name(&body.target)?;
    if body.target == ns {
        return Err(ApiError::bad_request(
            "copy target must differ from the source namespace",
        ));
    }

    let src = ns_ref(&org, &project, &ns);
    let dst = ns_ref(&org, &project, &body.target);
    let result = state.engine.copy_namespace(&src, &dst, true).await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "namespace.copy",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 201 } else { 409 },
        Some(serde_json::json!({ "target": body.target })),
    );

    Ok((StatusCode::CREATED, Json(result?)))
}
