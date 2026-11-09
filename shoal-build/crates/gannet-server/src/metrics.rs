//! Prometheus metrics registry for the API server.
//!
//! Metric names are prefixed with `gannet_` and follow Prometheus naming
//! conventions. Per-route labels use the *matched route template* (e.g.
//! `/v1/health`), never raw request paths, to keep label cardinality bounded.

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};

pub struct Metrics {
    registry: Registry,
    /// Total HTTP requests by method, matched route, and response status.
    pub http_requests_total: IntCounterVec,
    /// HTTP request latency by method and matched route.
    pub http_request_duration_seconds: HistogramVec,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "gannet_http_requests_total",
                "Total number of HTTP requests handled, by method, route, and status code.",
            ),
            &["method", "path", "status"],
        )
        .expect("valid metric definition");

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "gannet_http_request_duration_seconds",
                "HTTP request handling latency in seconds, by method and route.",
            )
            .buckets(vec![
                0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["method", "path"],
        )
        .expect("valid metric definition");

        let build_info = IntGaugeVec::new(
            Opts::new(
                "gannet_build_info",
                "Build information; the gauge is always 1 with version metadata in labels.",
            ),
            &["version"],
        )
        .expect("valid metric definition");
        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        registry
            .register(Box::new(http_requests_total.clone()))
            .expect("register http_requests_total");
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .expect("register http_request_duration_seconds");
        registry
            .register(Box::new(build_info))
            .expect("register build_info");

        Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
        }
    }

    /// Render all registered metrics in Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("encode prometheus metrics");
        String::from_utf8(buf).expect("prometheus text format is valid utf-8")
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_always_present() {
        let m = Metrics::new();
        let text = m.encode();
        assert!(text.contains("gannet_build_info"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn request_counter_appears_after_increment() {
        let m = Metrics::new();
        m.http_requests_total
            .with_label_values(&["GET", "/v1/health", "200"])
            .inc();
        let text = m.encode();
        assert!(text.contains("gannet_http_requests_total"));
        assert!(text.contains("path=\"/v1/health\""));
    }
}
