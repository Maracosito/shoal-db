//! Shared application state handed to every request handler.

use std::sync::Arc;
use std::time::Instant;

use crate::config::ServerConfig;
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ServerConfig>,
    pub metrics: Arc<Metrics>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            metrics: Arc::new(Metrics::new()),
            started_at: Instant::now(),
        }
    }

    /// Seconds since this process started serving.
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
