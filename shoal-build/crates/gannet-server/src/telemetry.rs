//! Structured logging initialization.
//!
//! `RUST_LOG`, when set, takes precedence over the configured `log.level`,
//! which is useful for ad-hoc debugging without touching config files.

use tracing_subscriber::EnvFilter;

use crate::config::{LogFormat, LogSection};

/// Initialize the global tracing subscriber. Safe to call more than once;
/// subsequent calls are no-ops (relevant for tests).
pub fn init(log: &LogSection) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new(&log.level).unwrap_or_else(|_| EnvFilter::new("info")));

    let result = match log.format {
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_env_filter(filter)
            .with_target(true)
            .try_init(),
        LogFormat::Text => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .try_init(),
    };

    // A second initialization (e.g. in tests) is not an error condition.
    let _ = result;
}
