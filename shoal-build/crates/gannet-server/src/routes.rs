//! HTTP route handlers and the metrics middleware.

use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct ServiceInfo {
    name: &'static str,
    version: &'static str,
    description: &'static str,
    api_base: &'static str,
    docs: &'static str,
}

/// `GET /` — service banner for humans and probes.
pub async fn root() -> Json<ServiceInfo> {
    Json(ServiceInfo {
        name: "gannet",
        version: env!("CARGO_PKG_VERSION"),
        description: "Object-storage-native vector + full-text search database",
        api_base: "/v1",
        docs: "https://github.com/gannet-db/gannet",
    })
}

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
    version: &'static str,
    uptime_seconds: u64,
    storage_backend: &'static str,
}

/// `GET /v1/health` — overall service health.
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.uptime_seconds(),
        storage_backend: state.config.storage.kind(),
    })
}

#[derive(Serialize)]
pub struct ProbeResponse {
    status: &'static str,
}

/// `GET /v1/health/live` — liveness probe: the process is up and serving.
pub async fn liveness() -> Json<ProbeResponse> {
    Json(ProbeResponse { status: "ok" })
}

/// `GET /v1/health/ready` — readiness probe.
///
/// In milestone 1 the server has no engine to warm up, so readiness mirrors
/// liveness. Once the engine is wired in (milestone 2), readiness will verify
/// object-storage connectivity before reporting `ok`.
pub async fn readiness() -> Json<ProbeResponse> {
    Json(ProbeResponse { status: "ok" })
}

/// `GET /metrics` — Prometheus text exposition.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.encode(),
    )
}

/// Middleware: record request count and latency for every request.
///
/// Uses the matched route template as the `path` label to keep metric
/// cardinality bounded; unmatched requests (404s) are bucketed together.
pub async fn track_http_metrics(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_owned());

    let start = Instant::now();
    let response = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    state
        .metrics
        .http_requests_total
        .with_label_values(&[method.as_str(), &path, &status])
        .inc();
    state
        .metrics
        .http_request_duration_seconds
        .with_label_values(&[method.as_str(), &path])
        .observe(elapsed);

    response
}
