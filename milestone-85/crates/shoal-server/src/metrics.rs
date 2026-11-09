//! Prometheus metrics for the Shoal server.
//!
//! Naming follows Prometheus conventions with the `shoal_` prefix. Namespace-
//! labelled gauges (`wal_bytes`, `segment_count`, `indexing_lag_seconds`) are
//! suitable for deployments with up to a few thousand namespaces; beyond that,
//! disable per-namespace gauges at the scrape config level.

use std::sync::Arc;

use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};

fn latency_buckets() -> Vec<f64> {
    vec![
        0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ]
}

pub struct Metrics {
    pub registry: Registry,

    // HTTP
    pub http_requests_total: IntCounterVec,
    pub http_request_duration_seconds: HistogramVec,

    // Query path
    pub query_duration_seconds: HistogramVec,

    // Cache hierarchy
    pub cache_hits_total: IntCounterVec,
    pub cache_misses_total: IntCounter,
    pub cache_evictions_total: IntCounterVec,
    pub cache_bytes: IntGaugeVec,

    // Object storage I/O
    pub object_store_ops_total: IntCounterVec,
    pub object_store_bytes_total: IntCounterVec,

    // Write/index path
    pub wal_bytes: IntGaugeVec,
    pub wal_records: IntGaugeVec,
    pub segment_count: IntGaugeVec,
    pub indexing_lag_seconds: GaugeVec,
    pub compaction_running: IntGauge,
    pub compactions_total: IntCounterVec,

    // Control plane
    pub namespaces_total: IntGauge,
    pub warm_requests_total: IntCounter,
    pub rate_limited_total: IntCounter,
    pub audit_events_total: IntCounter,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();

        fn counter_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
            let c = IntCounterVec::new(Opts::new(name, help), labels).expect("metric definition");
            r.register(Box::new(c.clone())).expect("metric registration");
            c
        }
        fn counter(r: &Registry, name: &str, help: &str) -> IntCounter {
            let c = IntCounter::new(name, help).expect("metric definition");
            r.register(Box::new(c.clone())).expect("metric registration");
            c
        }
        fn gauge_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> IntGaugeVec {
            let g = IntGaugeVec::new(Opts::new(name, help), labels).expect("metric definition");
            r.register(Box::new(g.clone())).expect("metric registration");
            g
        }
        fn fgauge_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> GaugeVec {
            let g = GaugeVec::new(Opts::new(name, help), labels).expect("metric definition");
            r.register(Box::new(g.clone())).expect("metric registration");
            g
        }
        fn gauge(r: &Registry, name: &str, help: &str) -> IntGauge {
            let g = IntGauge::new(name, help).expect("metric definition");
            r.register(Box::new(g.clone())).expect("metric registration");
            g
        }
        fn histogram_vec(r: &Registry, name: &str, help: &str, labels: &[&str]) -> HistogramVec {
            let h = HistogramVec::new(
                HistogramOpts::new(name, help).buckets(latency_buckets()),
                labels,
            )
            .expect("metric definition");
            r.register(Box::new(h.clone())).expect("metric registration");
            h
        }

        let m = Metrics {
            http_requests_total: counter_vec(
                &registry,
                "shoal_http_requests_total",
                "Total HTTP requests by method, route template and status",
                &["method", "route", "status"],
            ),
            http_request_duration_seconds: histogram_vec(
                &registry,
                "shoal_http_request_duration_seconds",
                "HTTP request latency by route template",
                &["route"],
            ),
            query_duration_seconds: histogram_vec(
                &registry,
                "shoal_query_duration_seconds",
                "Query execution latency by query kind (vector|fulltext|hybrid|filter)",
                &["kind"],
            ),
            cache_hits_total: counter_vec(
                &registry,
                "shoal_cache_hits_total",
                "Cache hits by tier (memory|disk)",
                &["tier"],
            ),
            cache_misses_total: counter(
                &registry,
                "shoal_cache_misses_total",
                "Full cache misses served from object storage",
            ),
            cache_evictions_total: counter_vec(
                &registry,
                "shoal_cache_evictions_total",
                "Cache evictions by tier (memory|disk)",
                &["tier"],
            ),
            cache_bytes: gauge_vec(
                &registry,
                "shoal_cache_bytes",
                "Current cache usage in bytes by tier (memory|disk)",
                &["tier"],
            ),
            object_store_ops_total: counter_vec(
                &registry,
                "shoal_object_store_ops_total",
                "Object storage operations by type (get|put|delete|list)",
                &["op"],
            ),
            object_store_bytes_total: counter_vec(
                &registry,
                "shoal_object_store_bytes_total",
                "Object storage payload bytes by direction (read|write)",
                &["direction"],
            ),
            wal_bytes: gauge_vec(
                &registry,
                "shoal_wal_bytes",
                "Unindexed write-ahead-log bytes per namespace",
                &["namespace"],
            ),
            wal_records: gauge_vec(
                &registry,
                "shoal_wal_records",
                "Unindexed write-ahead-log records per namespace",
                &["namespace"],
            ),
            segment_count: gauge_vec(
                &registry,
                "shoal_segment_count",
                "Live segments per namespace",
                &["namespace"],
            ),
            indexing_lag_seconds: fgauge_vec(
                &registry,
                "shoal_indexing_lag_seconds",
                "Age of the oldest unindexed write per namespace",
                &["namespace"],
            ),
            compaction_running: gauge(
                &registry,
                "shoal_compaction_running",
                "1 while a compaction is in progress on this node",
            ),
            compactions_total: counter_vec(
                &registry,
                "shoal_compactions_total",
                "Completed compactions by outcome (success|error)",
                &["outcome"],
            ),
            namespaces_total: gauge(
                &registry,
                "shoal_namespaces_total",
                "Total namespaces known to this node",
            ),
            warm_requests_total: counter(
                &registry,
                "shoal_warm_requests_total",
                "Warm-cache requests served",
            ),
            rate_limited_total: counter(
                &registry,
                "shoal_rate_limited_total",
                "Requests rejected by rate limiting",
            ),
            audit_events_total: counter(
                &registry,
                "shoal_audit_events_total",
                "Audit events recorded",
            ),
            registry,
        };
        Arc::new(m)
    }

    /// Render the registry in Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&self.registry.gather(), &mut buf) {
            tracing::warn!(error = %e, "failed to encode metrics");
        }
        String::from_utf8(buf).unwrap_or_default()
    }

    pub fn observe_http(&self, method: &str, route: &str, status: u16, seconds: f64) {
        self.http_requests_total
            .with_label_values(&[method, route, &status.to_string()])
            .inc();
        self.http_request_duration_seconds
            .with_label_values(&[route])
            .observe(seconds);
    }

    pub fn observe_query(&self, kind: &str, seconds: f64) {
        self.query_duration_seconds
            .with_label_values(&[kind])
            .observe(seconds);
    }

    /// Record which tier served a cached read ("memory" | "disk" | "origin").
    pub fn record_cache_tier(&self, tier: &str) {
        match tier {
            "origin" => self.cache_misses_total.inc(),
            t => self.cache_hits_total.with_label_values(&[t]).inc(),
        }
    }

    pub fn record_object_read(&self, bytes: u64) {
        self.object_store_ops_total.with_label_values(&["get"]).inc();
        self.object_store_bytes_total
            .with_label_values(&["read"])
            .inc_by(bytes);
    }

    pub fn record_object_write(&self, bytes: u64) {
        self.object_store_ops_total.with_label_values(&["put"]).inc();
        self.object_store_bytes_total
            .with_label_values(&["write"])
            .inc_by(bytes);
    }

    pub fn record_object_delete(&self) {
        self.object_store_ops_total
            .with_label_values(&["delete"])
            .inc();
    }

    pub fn record_object_list(&self) {
        self.object_store_ops_total
            .with_label_values(&["list"])
            .inc();
    }

    /// Update per-namespace write/index gauges in one call.
    pub fn set_namespace_state(
        &self,
        namespace: &str,
        wal_bytes: i64,
        wal_records: i64,
        segments: i64,
        indexing_lag_secs: f64,
    ) {
        self.wal_bytes
            .with_label_values(&[namespace])
            .set(wal_bytes);
        self.wal_records
            .with_label_values(&[namespace])
            .set(wal_records);
        self.segment_count
            .with_label_values(&[namespace])
            .set(segments);
        self.indexing_lag_seconds
            .with_label_values(&[namespace])
            .set(indexing_lag_secs);
    }

    /// Drop per-namespace gauges after a namespace is deleted.
    pub fn clear_namespace_state(&self, namespace: &str) {
        let labels = &[namespace];
        let _ = self.wal_bytes.remove_label_values(labels);
        let _ = self.wal_records.remove_label_values(labels);
        let _ = self.segment_count.remove_label_values(labels);
        let _ = self.indexing_lag_seconds.remove_label_values(labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_and_render() {
        let m = Metrics::new();
        m.observe_http("POST", "/v1/namespaces/{ns}/query", 200, 0.012);
        m.observe_query("hybrid", 0.010);
        m.record_cache_tier("memory");
        m.record_cache_tier("disk");
        m.record_cache_tier("origin");
        m.record_object_read(1024);
        m.record_object_write(2048);
        m.set_namespace_state("docs", 100, 3, 5, 1.5);

        let text = m.render();
        assert!(text.contains("shoal_http_requests_total"));
        assert!(text.contains("shoal_query_duration_seconds"));
        assert!(text.contains("shoal_cache_hits_total"));
        assert!(text.contains("shoal_cache_misses_total"));
        assert!(text.contains("shoal_object_store_bytes_total"));
        assert!(text.contains("shoal_wal_bytes"));
        assert!(text.contains("shoal_segment_count"));
        assert!(text.contains("shoal_indexing_lag_seconds"));
    }

    #[test]
    fn namespace_gauges_can_be_cleared() {
        let m = Metrics::new();
        m.set_namespace_state("temp", 1, 1, 1, 0.0);
        assert!(m.render().contains("namespace=\"temp\""));
        m.clear_namespace_state("temp");
        assert!(!m.render().contains("namespace=\"temp\""));
    }

    #[test]
    fn independent_instances_do_not_conflict() {
        let _a = Metrics::new();
        let _b = Metrics::new(); // each has its own Registry; no global state
    }
}
