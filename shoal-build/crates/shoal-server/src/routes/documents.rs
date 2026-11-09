//! Document write handlers: upsert (row + columnar), patch, delete.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use shoal_core::filter::Filter;
use shoal_core::types::Document;

use crate::auth::{Identity, Role};
use crate::engine::{DocumentPatch, WriteCondition, WriteOptions, WriteResult};
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

use super::{authorize, ns_ref, record_audit};

// ---------------------------------------------------------------------------
// Upsert
// ---------------------------------------------------------------------------

/// Upsert request body. Documents may be provided row-oriented
/// (`documents: [...]`), column-oriented (`columns: {field: [values...]}`),
/// or both in one request (rows are appended after the pivoted columns).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertRequest {
    /// Row-oriented documents.
    #[serde(default)]
    pub documents: Vec<Value>,
    /// Column-oriented batch: a map of field name to a column of values.
    /// All columns must be the same length and an `id` column is required.
    #[serde(default)]
    pub columns: Option<BTreeMap<String, Vec<Value>>>,
    /// Optional conditional-write predicate evaluated atomically by the
    /// engine (e.g. only-if-absent or version match).
    #[serde(default)]
    pub condition: Option<WriteCondition>,
    /// Optional idempotency token. Replays with the same token are
    /// acknowledged without being re-applied.
    #[serde(default)]
    pub request_token: Option<String>,
}

/// Pivot a columnar batch into row-oriented JSON objects.
fn pivot_columns(columns: BTreeMap<String, Vec<Value>>) -> Result<Vec<Value>, ApiError> {
    if columns.is_empty() {
        return Ok(Vec::new());
    }
    if !columns.contains_key("id") {
        return Err(ApiError::bad_request(
            "columnar batches must include an `id` column",
        ));
    }
    let len = columns.values().next().map(|c| c.len()).unwrap_or(0);
    for (name, col) in &columns {
        if col.len() != len {
            return Err(ApiError::bad_request(format!(
                "column `{name}` has length {} but expected {len}",
                col.len()
            )));
        }
    }
    let mut rows = Vec::with_capacity(len);
    for i in 0..len {
        let mut obj = Map::new();
        for (name, col) in &columns {
            let value = col[i].clone();
            // Nulls in a column mean "field absent for this row".
            if !value.is_null() {
                obj.insert(name.clone(), value);
            }
        }
        rows.push(Value::Object(obj));
    }
    Ok(rows)
}

/// Deserialize raw JSON rows into typed documents with index-tagged errors.
fn parse_documents(rows: Vec<Value>) -> Result<Vec<Document>, ApiError> {
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            serde_json::from_value::<Document>(row)
                .map_err(|e| ApiError::bad_request(format!("invalid document at index {i}: {e}")))
        })
        .collect()
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/documents`
pub async fn upsert(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<UpsertRequest>,
) -> Result<Json<WriteResult>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;

    let mut rows = match body.columns {
        Some(columns) => pivot_columns(columns)?,
        None => Vec::new(),
    };
    rows.extend(body.documents);
    if rows.is_empty() {
        return Err(ApiError::bad_request(
            "upsert requires `documents` (rows) and/or `columns` (columnar batch)",
        ));
    }

    let docs = parse_documents(rows)?;
    let count = docs.len();

    let options = WriteOptions {
        condition: body.condition,
        request_token: body.request_token,
        ..Default::default()
    };

    let result = state
        .engine
        .upsert(&ns_ref(&org, &project, &ns), docs, options)
        .await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "documents.upsert",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 400 },
        Some(json!({ "documents": count })),
    );

    Ok(Json(result?))
}

// ---------------------------------------------------------------------------
// Patch
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    pub patches: Vec<DocumentPatch>,
}

/// `PATCH /v1/orgs/:org/projects/:project/namespaces/:ns/documents`
///
/// Partially updates existing documents: each patch carries `set` fields to
/// merge and `unset` fields to remove. Patching a missing document is an
/// error surfaced by the engine in the per-batch result.
pub async fn patch(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<PatchRequest>,
) -> Result<Json<WriteResult>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;
    if body.patches.is_empty() {
        return Err(ApiError::bad_request("`patches` must not be empty"));
    }
    let count = body.patches.len();

    let result = state
        .engine
        .patch(&ns_ref(&org, &project, &ns), body.patches, WriteOptions::default())
        .await;
    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        "documents.patch",
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 400 },
        Some(json!({ "patches": count })),
    );

    Ok(Json(result?))
}

// ---------------------------------------------------------------------------
// Delete (by IDs or by filter)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub filter: Option<Filter>,
}

/// `POST /v1/orgs/:org/projects/:project/namespaces/:ns/documents/delete`
///
/// Exactly one of `ids` or `filter` must be supplied. Filter deletes are
/// resolved against the current snapshot then logged as tombstones, so the
/// operation is durable before it is acknowledged.
pub async fn delete(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    Extension(request_id): Extension<RequestId>,
    Path((org, project, ns)): Path<(String, String, String)>,
    Json(body): Json<DeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&identity, &org, &project, Role::Writer)?;

    let nref = ns_ref(&org, &project, &ns);
    let (result, action, detail) = match (body.ids.is_empty(), body.filter) {
        (false, None) => {
            let count = body.ids.len();
            (
                state.engine.delete_ids(&nref, body.ids).await.map(|wr| (wr.records, Some(wr))),
                "documents.delete",
                json!({ "requested_ids": count }),
            )
        }
        (true, Some(filter)) => (
            state.engine.delete_by_filter(&nref, &filter, WriteOptions::default()).await,
            "documents.delete_by_filter",
            json!({}),
        ),
        (false, Some(_)) => {
            return Err(ApiError::bad_request(
                "supply either `ids` or `filter`, not both",
            ))
        }
        (true, None) => {
            return Err(ApiError::bad_request(
                "supply `ids` (non-empty) or `filter`",
            ))
        }
    };

    let success = result.is_ok();
    record_audit(
        &state,
        &identity,
        &request_id,
        action,
        &org,
        &project,
        Some(&ns),
        success,
        if success { 200 } else { 400 },
        Some(detail),
    );

    let deleted = result?;
    Ok(Json(json!({ "deleted": deleted })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pivot_produces_one_object_per_row() {
        let mut cols = BTreeMap::new();
        cols.insert("id".to_owned(), vec![json!("a"), json!("b")]);
        cols.insert("title".to_owned(), vec![json!("first"), json!("second")]);
        cols.insert("rank".to_owned(), vec![json!(1), Value::Null]);

        let rows = pivot_columns(cols).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], "a");
        assert_eq!(rows[0]["rank"], 1);
        // Null column entries are dropped, not serialized as null.
        assert!(rows[1].get("rank").is_none());
        assert_eq!(rows[1]["title"], "second");
    }

    #[test]
    fn pivot_rejects_missing_id_column() {
        let mut cols = BTreeMap::new();
        cols.insert("title".to_owned(), vec![json!("x")]);
        assert!(pivot_columns(cols).is_err());
    }

    #[test]
    fn pivot_rejects_ragged_columns() {
        let mut cols = BTreeMap::new();
        cols.insert("id".to_owned(), vec![json!("a"), json!("b")]);
        cols.insert("title".to_owned(), vec![json!("only-one")]);
        assert!(pivot_columns(cols).is_err());
    }

    #[test]
    fn empty_columns_pivot_to_no_rows() {
        assert!(pivot_columns(BTreeMap::new()).unwrap().is_empty());
    }
}
