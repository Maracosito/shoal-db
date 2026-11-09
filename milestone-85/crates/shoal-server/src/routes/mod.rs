//! HTTP route table and request middleware.
//!
//! ## Middleware stack (outermost first)
//!
//! 1. body-size limit (`DefaultBodyLimit`)
//! 2. timeout
//! 3. HTTP tracing (`TraceLayer`)
//! 4. request-ID assignment ([`crate::request_id`])
//! 5. Prometheus HTTP metrics
//! 6. API-key authentication (protected routes only)
//! 7. per-key rate limiting (protected routes only)
//!
//! `/healthz`, `/readyz` and `/metrics` are intentionally outside the auth
//! boundary so probes and scrapers need no credentials; deployment hardening
//! docs cover restricting them at the network layer.
//!
//! ## Engine façade contract
//!
//! Handlers in this module tree call the engine façade defined in
//! `crate::engine` (see `engine/mod.rs`):
//!
//! * namespaces: `create_namespace`, `list_namespaces`, `describe_namespace`,
//!   `update_namespace`, `delete_namespace`
//! * writes: `upsert`, `patch`, `delete_ids`, `delete_by_filter`
//! * reads: `query`, `export_page`
//! * branching: `branch_namespace`, `list_branches`, `copy_namespace`
//! * cache ops: `warm`, `pin`, `unpin`
//! * maintenance: `compact`, `repair`, `health_check`

use std::time::{Duration, Instant};

use axum::{
    extract::{DefaultBodyLimit, MatchedPath, Request, State},
    http::header,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::Value;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{Identity, Role};
use crate::error::ApiError;
use crate::request_id::RequestId;
use crate::AppState;

pub mod branch;
pub mod documents;
pub mod export;
pub mod namespaces;
pub mod ops;
pub mod query;
pub mod system;

/// Maximum request body size. Large ingests should be split into batches;
/// the export path streams and is unaffected by this limit.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Per-request timeout. Export streams begin responding immediately, so this
/// effectively bounds query/write/compaction handler time.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Assemble the full router.
pub fn build(state: AppState) -> Router {
    let ns_routes = Router::new()
        .route("/", post(namespaces::create).get(namespaces::list))
        .route(
            "/:ns",
            get(namespaces::describe)
                .patch(namespaces::update)
                .delete(namespaces::remove),
        )
        .route(
            "/:ns/documents",
            post(documents::upsert).patch(documents::patch),
        )
        .route("/:ns/documents/delete", post(documents::delete))
        .route("/:ns/query", post(query::query))
        .route("/:ns/multi-query", post(query::multi_query))
        .route("/:ns/export", get(export::export))
        .route("/:ns/copy", post(export::copy))
        .route("/:ns/branch", post(branch::create))
        .route("/:ns/branches", get(branch::list))
        .route("/:ns/warm", post(ops::warm))
        .route("/:ns/pin", post(ops::pin).delete(ops::unpin))
        .route("/:ns/compact", post(ops::compact))
        .route("/:ns/repair", post(ops::repair));

    let protected = Router::new()
        .nest("/v1/orgs/:org/projects/:project/namespaces", ns_routes)
        // Inner-to-outer: rate limit runs after auth (it is keyed by key ID).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .merge(protected)
        .route("/healthz", get(system::healthz))
        .route("/readyz", get(system::readyz))
        .route("/metrics", get(system::metrics))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            metrics_middleware,
        ))
        .layer(middleware::from_fn(crate::request_id::middleware))
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(REQUEST_TIMEOUT))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Pull the API key out of `Authorization: Bearer <key>` or `x-api-key`.
fn extract_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")) {
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Authenticate the request and stash the resolved [`Identity`] in request
/// extensions. The raw key is never logged or stored anywhere downstream.
async fn auth_middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(token) = extract_token(req.headers()) else {
        return ApiError::unauthorized(
            "missing API key: supply `Authorization: Bearer <key>` or `x-api-key`",
        )
        .into_response();
    };

    match state.auth.authenticate(&token) {
        Some(identity) => {
            req.extensions_mut().insert(identity);
            next.run(req).await
        }
        None => ApiError::unauthorized("invalid API key").into_response(),
    }
}

/// Per-API-key rate limiting. Disabled limits (the default) always allow.
async fn rate_limit_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = req
        .extensions()
        .get::<Identity>()
        .map(|i| i.key_id.clone())
        .unwrap_or_else(|| "anonymous".to_owned());

    match state.limiter.try_acquire(&key) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => {
            let mut res =
                ApiError::too_many_requests("rate limit exceeded for this API key").into_response();
            let secs = retry_after.as_secs().max(1).to_string();
            if let Ok(value) = secs.parse() {
                res.headers_mut().insert(header::RETRY_AFTER, value);
            }
            res
        }
    }
}

/// Record HTTP request count/latency per (method, route-template, status).
/// Uses the matched route template (e.g. `/:ns/query`) rather than the raw
/// path so cardinality stays bounded and namespace names never become labels.
async fn metrics_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    let start = Instant::now();
    let res = next.run(req).await;
    state
        .metrics
        .observe_http_request(method.as_str(), &route, res.status().as_u16(), start.elapsed());
    res
}

// ---------------------------------------------------------------------------
// Shared handler helpers
// ---------------------------------------------------------------------------

/// Check that the key is scoped to this org/project and holds at least the
/// required role. Returns 403 with a non-leaky message otherwise.
pub(crate) fn authorize(
    identity: &Identity,
    org: &str,
    project: &str,
    required: Role,
) -> Result<(), ApiError> {
    if !identity.can_access(org, project) {
        return Err(ApiError::forbidden(format!(
            "API key is not scoped to org `{org}` / project `{project}`"
        )));
    }
    if !identity.role.allows(required) {
        return Err(ApiError::forbidden(format!(
            "operation requires the `{required:?}` role; this key has `{:?}`",
            identity.role
        )));
    }
    Ok(())
}

/// Build a [`crate::engine::NamespaceRef`] from path components.
pub(crate) fn ns_ref(org: &str, project: &str, ns: &str) -> crate::engine::NamespaceRef {
    crate::engine::NamespaceRef::new(org, project, ns)
}

/// Validate user-supplied namespace names. Names become object-storage key
/// components, so the character set is deliberately conservative.
pub(crate) fn validate_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "namespace names must be 1-128 characters of [A-Za-z0-9._-] and must not start with `.`",
        ))
    }
}

/// Emit an audit record for a state-changing operation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_audit(
    state: &AppState,
    identity: &Identity,
    request_id: &RequestId,
    action: &str,
    org: &str,
    project: &str,
    namespace: Option<&str>,
    success: bool,
    status: u16,
    detail: Option<Value>,
) {
    state.audit.record(crate::audit::AuditEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        request_id: request_id.0.clone(),
        actor: identity.key_id.clone(),
        role: format!("{:?}", identity.role),
        action: action.to_owned(),
        org: org.to_owned(),
        project: project.to_owned(),
        namespace: namespace.map(str::to_owned),
        success,
        status,
        detail,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn extracts_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sk_test_123"),
        );
        assert_eq!(extract_token(&headers).as_deref(), Some("sk_test_123"));
    }

    #[test]
    fn extracts_x_api_key_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("sk_test_456"));
        assert_eq!(extract_token(&headers).as_deref(), Some("sk_test_456"));
    }

    #[test]
    fn missing_or_empty_tokens_are_none() {
        let headers = HeaderMap::new();
        assert!(extract_token(&headers).is_none());

        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert!(extract_token(&headers).is_none());
    }

    #[test]
    fn namespace_name_validation() {
        assert!(validate_name("docs").is_ok());
        assert!(validate_name("my-ns_2.prod").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("has/slash").is_err());
        assert!(validate_name(&"a".repeat(129)).is_err());
    }
}
