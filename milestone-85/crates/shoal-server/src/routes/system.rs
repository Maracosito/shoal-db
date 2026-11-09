//! Unauthenticated system endpoints: liveness, readiness, metrics.

use std::sync::OnceLock;
use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    Json,
};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::AppState;

static STARTED: OnceLock<Instant> = OnceLock::new();

/// Record process start time; called once from [`crate::run`].
pub fn mark_started() {
    let _ = STARTED.set(Instant::now());
}

/// `GET /healthz` — liveness. Always 200 while the process can serve HTTP.
pub async fn healthz() -> Json<Value> {
    let uptime = STARTED.get().map(|s| s.elapsed().as_secs()).unwrap_or(0);
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
        "uptime_seconds": uptime,
    }))
}

/// `GET /readyz` — readiness. Verifies the engine can reach object storage
/// (a cheap metadata round-trip), so load balancers stop routing to nodes
/// that have lost their backend.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    state.engine.health_check().await?;
    Ok(Json(json!({ "status": "ready" })))
}

/// `GET /metrics` — Prometheus text exposition (format 0.0.4).
pub async fn metrics(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Body::from(body))
        .expect("static response parts are valid")
}
