//! Structured logging and optional OpenTelemetry trace export.
//!
//! Logging is `tracing`-based with either human-readable or JSON output.
//! Secrets discipline: handlers never put credentials in span fields; the
//! `Authorization` header is excluded from request logging at the middleware
//! level, and audit events carry only key *ids* (see [`crate::audit`]).
//!
//! OTLP export is compiled in only with the `otel` cargo feature to keep the
//! default build lean.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::config::TelemetryConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

impl Default for LogFormat {
    fn default() -> Self {
        LogFormat::Pretty
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" | "text" | "human" => Ok(LogFormat::Pretty),
            other => Err(format!("unknown log format '{other}'")),
        }
    }
}

/// Initialize the global tracing subscriber. Call once at process start.
pub fn init(cfg: &TelemetryConfig) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.log_level.clone()));

    let fmt_layer: Box<dyn Layer<_> + Send + Sync> = match cfg.log_format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_target(true)
            .with_current_span(true)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer().with_target(true).boxed(),
    };

    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    #[cfg(feature = "otel")]
    {
        if let Some(endpoint) = &cfg.otlp_endpoint {
            let tracer = build_otlp_tracer(endpoint, &cfg.service_name)?;
            registry
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .try_init()?;
            tracing::info!(endpoint, "OpenTelemetry OTLP trace export enabled");
            return Ok(());
        }
        registry.try_init()?;
        return Ok(());
    }

    #[cfg(not(feature = "otel"))]
    {
        if cfg.otlp_endpoint.is_some() {
            eprintln!(
                "warning: telemetry.otlp_endpoint is set but shoal-server was built \
                 without the `otel` feature; trace export is disabled"
            );
        }
        registry.try_init()?;
        Ok(())
    }
}

#[cfg(feature = "otel")]
fn build_otlp_tracer(
    endpoint: &str,
    service_name: &str,
) -> anyhow::Result<opentelemetry_sdk::trace::Tracer> {
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(endpoint.to_string()),
        )
        .with_trace_config(sdktrace::Config::default().with_resource(Resource::new(vec![
            KeyValue::new("service.name", service_name.to_string()),
        ])))
        .install_batch(runtime::Tokio)?;
    Ok(tracer)
}

/// Quiet initializer for tests; safe to call repeatedly.
pub fn init_for_tests() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn"))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parses() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("TEXT".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert!("xml".parse::<LogFormat>().is_err());
    }
}
