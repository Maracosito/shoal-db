//! Gannet API server library.
//!
//! This crate exposes the HTTP application as a reusable [`axum::Router`] so
//! that the binary entry point, integration tests, and (later) the embedded
//! dev-mode CLI can all construct the exact same application.
//!
//! Milestone 1 scope: configuration loading, structured logging, Prometheus
//! metrics, and the health/metrics endpoints. The namespace/document/query
//! routes defined in `api/openapi.yaml` are wired up in milestone 2 once the
//! engine crate exposes the write and query paths.

pub mod config;
pub mod metrics;
pub mod routes;
pub mod state;
pub mod telemetry;

use axum::{middleware, routing::get, Router};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the complete HTTP application with all routes and middleware.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(routes::root))
        .route("/v1/health", get(routes::health))
        .route("/v1/health/live", get(routes::liveness))
        .route("/v1/health/ready", get(routes::readiness))
        .route("/metrics", get(routes::metrics))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            routes::track_http_metrics,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
