//! Shoal API server.
//!
//! This crate wires the storage/query engine (`shoal-core`) and the cache
//! hierarchy (`shoal-cache`) behind a production HTTP layer:
//!
//! * API-key authentication with an admin / writer / reader role model
//!   ([`auth`]).
//! * The full JSON API surface — namespace CRUD, document writes (row and
//!   columnar), queries, export, copy, branching, warming, pinning,
//!   compaction and repair ([`routes`]).
//! * Structured logging, Prometheus metrics, OpenTelemetry tracing hooks,
//!   audit logging, request IDs and optional rate limits ([`metrics`],
//!   [`telemetry`], [`audit`], [`request_id`], [`rate_limit`]).
//!
//! The server itself is stateless apart from its caches: every durable byte
//! lives in object storage and any node can be replaced by a fresh one.

pub mod audit;
pub mod auth;
pub mod config;
pub mod engine;
pub mod error;
pub mod metrics;
pub mod rate_limit;
pub mod request_id;
pub mod routes;
pub mod telemetry;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;

use crate::audit::AuditLogger;
use crate::auth::AuthStore;
use crate::config::ServerConfig;
use crate::engine::Engine;
use crate::metrics::Metrics;
use crate::rate_limit::RateLimiter;

/// Crate version, surfaced on `/healthz` and in the `server` response header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared application state cloned into every request handler.
///
/// Everything inside is behind an `Arc`, so cloning `AppState` is cheap and
/// the whole struct is `Send + Sync`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub engine: Arc<Engine>,
    pub auth: Arc<AuthStore>,
    pub metrics: Arc<Metrics>,
    pub audit: Arc<AuditLogger>,
    pub limiter: Arc<RateLimiter>,
}

/// Construct the full application state from a loaded configuration.
///
/// This opens the object store, replays any unindexed WAL entries, loads the
/// API-key store and initialises metrics, audit and rate-limiting subsystems.
pub async fn build_state(config: ServerConfig) -> anyhow::Result<AppState> {
    let config = Arc::new(config);

    let metrics = Metrics::new();

    let engine = Arc::new(
        Engine::open(config.clone(), metrics.clone())
            .await
            .context("opening storage engine")?,
    );

    let auth = if let Some(ref root_key) = config.auth.root_api_key {
        // Root API key provided — create store that accepts it as admin.
        Arc::new(AuthStore::with_root_key(root_key.clone()))
    } else if config.auth.allow_anonymous {
        Arc::new(AuthStore::empty())
    } else {
        let (store, bootstrap_token) = AuthStore::load_or_bootstrap(
            &config.auth.keys_file,
            &config.auth.bootstrap_org,
        ).context("loading API key store")?;
        if let Some(token) = bootstrap_token {
            tracing::warn!("Bootstrap admin key generated: {token}");
        }
        Arc::new(store)
    };

    let audit = match AuditLogger::new(config.audit.path.as_deref()) {
        Ok(logger) => Arc::new(logger),
        Err(e) => {
            tracing::warn!("Audit log unavailable ({e}), running without audit");
            Arc::new(AuditLogger::new(None).expect("no-op audit logger"))
        }
    };

    let limiter = Arc::new(RateLimiter::new(config.limits.rate_limit.clone()));

    Ok(AppState {
        config,
        engine,
        auth,
        metrics,
        audit,
        limiter,
    })
}

/// Build the Axum router for the given state.
///
/// Exposed publicly so integration tests can drive the router in-process via
/// `tower::ServiceExt::oneshot` without binding a TCP socket.
pub fn build_router(state: AppState) -> axum::Router {
    routes::build(state)
}

/// Run the server until SIGINT/SIGTERM.
pub async fn run(config: ServerConfig) -> anyhow::Result<()> {
    let listen_addr = config.listen.clone();
    let state = build_state(config).await?;

    routes::system::mark_started();

    let addr: SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid listen address `{listen_addr}`"))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(%addr, version = VERSION, "shoal-server listening");

    let app = build_router(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving HTTP")?;

    tracing::info!("shoal-server shut down cleanly");
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
