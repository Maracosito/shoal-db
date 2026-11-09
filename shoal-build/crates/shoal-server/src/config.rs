//! Server configuration: defaults < TOML file < environment variables.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::rate_limit::{QuotaConfig, RateLimitConfig};
use crate::telemetry::LogFormat;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the HTTP server binds, e.g. "0.0.0.0:8765".
    pub listen: String,
    pub storage: StorageConfig,
    pub cache: CacheConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub telemetry: TelemetryConfig,
    pub audit: AuditConfig,
    pub indexer: IndexerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8765".to_string(),
            storage: StorageConfig::default(),
            cache: CacheConfig::default(),
            auth: AuthConfig::default(),
            limits: LimitsConfig::default(),
            telemetry: TelemetryConfig::default(),
            audit: AuditConfig::default(),
            indexer: IndexerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Object store URL. `file:///path` or `file://./relative` for the local
    /// filesystem backend; `s3://bucket/prefix` for S3-compatible storage
    /// (MinIO in development). S3 credentials come from the standard
    /// AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY environment variables and are
    /// never placed in this file.
    pub url: String,
    /// Custom S3 endpoint (e.g. "http://localhost:9000" for MinIO).
    pub s3_endpoint: Option<String>,
    pub s3_region: Option<String>,
    /// Path-style addressing, required by MinIO.
    pub s3_force_path_style: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            url: "file://./shoal-data".to_string(),
            s3_endpoint: None,
            s3_region: None,
            s3_force_path_style: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Disk cache directory. `None` disables the disk tier (memory only).
    pub dir: Option<PathBuf>,
    pub disk_max_bytes: u64,
    pub memory_max_bytes: u64,
    pub memory_max_item_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            dir: Some(PathBuf::from("./shoal-cache")),
            disk_max_bytes: 10 * 1024 * 1024 * 1024,  // 10 GiB
            memory_max_bytes: 512 * 1024 * 1024,      // 512 MiB
            memory_max_item_bytes: 8 * 1024 * 1024,   // 8 MiB
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Path to the API key file (TOML, hashed secrets only). If the file does
    /// not exist on startup, a bootstrap admin key is generated, the file is
    /// written with mode 0600, and the plaintext token is shown exactly once.
    pub keys_file: PathBuf,
    /// Organization the bootstrap admin key is scoped to.
    pub bootstrap_org: String,
    /// Development convenience: when true, requests without credentials get an
    /// implicit admin context. NEVER enable outside local development.
    pub allow_anonymous: bool,
    /// Optional root API key that grants admin access to all orgs/projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_api_key: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            keys_file: PathBuf::from("./shoal-keys.toml"),
            bootstrap_org: "default".to_string(),
            allow_anonymous: false,
            root_api_key: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub rate_limit: RateLimitConfig,
    pub quotas: QuotaConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub log_format: LogFormat,
    /// `tracing` env-filter directive, e.g. "info" or "info,shoal_server=debug".
    pub log_level: String,
    /// OTLP gRPC endpoint for trace export (requires the `otel` build feature).
    pub otlp_endpoint: Option<String>,
    pub service_name: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Pretty,
            log_level: "info".to_string(),
            otlp_endpoint: None,
            service_name: "shoal-server".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    pub enabled: bool,
    /// JSON-lines audit log path. `None` emits audit events only to the
    /// structured log (target "shoal::audit").
    pub path: Option<PathBuf>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: Some(PathBuf::from("./shoal-audit.log")),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexerConfig {
    /// How often the background indexer checks namespaces for unindexed WAL.
    pub interval_ms: u64,
    /// Compaction triggers once a namespace accumulates this many segments.
    pub compaction_min_segments: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            interval_ms: 2000,
            compaction_min_segments: 8,
        }
    }
}

impl ServerConfig {
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Load from an optional TOML file, then apply environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut cfg = match path {
            Some(p) => Self::from_toml_str(&std::fs::read_to_string(p)?)?,
            None => Self::default(),
        };
        cfg.apply_env();
        Ok(cfg)
    }

    /// Environment overrides (highest precedence). All variables are optional.
    pub fn apply_env(&mut self) {
        fn env(k: &str) -> Option<String> {
            std::env::var(k).ok().filter(|s| !s.is_empty())
        }
        if let Some(v) = env("SHOAL_LISTEN") {
            self.listen = v;
        }
        if let Some(v) = env("SHOAL_LISTEN_ADDR") {
            self.listen = v;
        }
        if let Some(v) = env("SHOAL_STORAGE_URL") {
            self.storage.url = v;
        }
        // Support SHOAL_STORAGE_BACKEND=s3 + SHOAL_S3_BUCKET=bucket
        if let Some(backend) = env("SHOAL_STORAGE_BACKEND") {
            if backend == "s3" {
                let bucket = env("SHOAL_S3_BUCKET").unwrap_or_else(|| "shoal".to_string());
                self.storage.url = format!("s3://{bucket}");
            }
        }
        if let Some(v) = env("SHOAL_S3_ENDPOINT") {
            self.storage.s3_endpoint = Some(v);
        }
        if let Some(v) = env("SHOAL_S3_REGION") {
            self.storage.s3_region = Some(v);
        }
        if let Some(v) = env("SHOAL_CACHE_DIR") {
            self.cache.dir = Some(PathBuf::from(v));
        }
        if let Some(v) = env("SHOAL_CACHE_DISK_MAX_BYTES").or_else(|| env("SHOAL_CACHE_DISK_BYTES")).and_then(|v| v.parse().ok()) {
            self.cache.disk_max_bytes = v;
        }
        if let Some(v) = env("SHOAL_CACHE_MEMORY_MAX_BYTES").or_else(|| env("SHOAL_CACHE_MEMORY_BYTES")).and_then(|v| v.parse().ok()) {
            self.cache.memory_max_bytes = v;
        }
        if let Some(v) = env("SHOAL_KEYS_FILE") {
            self.auth.keys_file = PathBuf::from(v);
        }
        if let Some(v) = env("SHOAL_ROOT_API_KEY") {
            self.auth.allow_anonymous = false;
            // Store root key for authentication
            self.auth.root_api_key = Some(v);
        }
        if let Some(v) = env("SHOAL_BOOTSTRAP_ORG") {
            self.auth.bootstrap_org = v;
        }
        if let Some(v) = env("SHOAL_ALLOW_ANONYMOUS") {
            self.auth.allow_anonymous = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = env("SHOAL_LOG_LEVEL").or_else(|| env("SHOAL_LOG")) {
            self.telemetry.log_level = v;
        }
        if let Some(v) = env("SHOAL_LOG_FORMAT") {
            if let Ok(f) = v.parse() {
                self.telemetry.log_format = f;
            }
        }
        if let Some(v) = env("SHOAL_OTLP_ENDPOINT") {
            self.telemetry.otlp_endpoint = Some(v);
        }
        if let Some(v) = env("SHOAL_AUDIT_PATH") {
            self.audit.path = Some(PathBuf::from(v));
        }
        if let Some(v) = env("SHOAL_RATE_LIMIT_ENABLED") {
            self.limits.rate_limit.enabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
    }

    /// Annotated example configuration (used by `shoal config init`).
    pub fn example_toml() -> &'static str {
        EXAMPLE_CONFIG
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# Shoal server configuration.
# Every value shown here is the default; env vars (SHOAL_*) override the file.
listen = "127.0.0.1:8765"

[storage]
# file:// for local development/tests, s3:// for production or MinIO.
url = "file://./shoal-data"
# s3_endpoint = "http://localhost:9000"   # MinIO
# s3_region = "us-east-1"
s3_force_path_style = true

[cache]
dir = "./shoal-cache"
disk_max_bytes = 10737418240      # 10 GiB
memory_max_bytes = 536870912      # 512 MiB
memory_max_item_bytes = 8388608   # 8 MiB

[auth]
keys_file = "./shoal-keys.toml"
bootstrap_org = "default"
allow_anonymous = false

[limits.rate_limit]
enabled = false
requests_per_second = 100.0
burst = 200.0
write_bytes_per_second = 0.0      # 0 = unlimited

[limits.quotas]
# max_namespaces_per_project = 1000
# max_batch_documents = 10000
# max_document_bytes = 1048576

[telemetry]
log_format = "pretty"             # or "json"
log_level = "info"
service_name = "shoal-server"
# otlp_endpoint = "http://localhost:4317"   # requires --features otel

[audit]
enabled = true
path = "./shoal-audit.log"

[indexer]
interval_ms = 2000
compaction_min_segments = 8
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses_and_matches_defaults() {
        let cfg = ServerConfig::from_toml_str(EXAMPLE_CONFIG).unwrap();
        let def = ServerConfig::default();
        assert_eq!(cfg.listen, def.listen);
        assert_eq!(cfg.storage.url, def.storage.url);
        assert_eq!(cfg.cache.disk_max_bytes, def.cache.disk_max_bytes);
        assert_eq!(cfg.limits.rate_limit.enabled, false);
        assert_eq!(cfg.indexer.compaction_min_segments, 8);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let cfg = ServerConfig::from_toml_str("").unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8765");
        assert!(cfg.audit.enabled);
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(ServerConfig::from_toml_str("nonsense_field = 1").is_err());
    }

    #[test]
    fn env_overrides_apply() {
        std::env::set_var("SHOAL_LISTEN", "0.0.0.0:9999");
        std::env::set_var("SHOAL_RATE_LIMIT_ENABLED", "true");
        let mut cfg = ServerConfig::default();
        cfg.apply_env();
        assert_eq!(cfg.listen, "0.0.0.0:9999");
        assert!(cfg.limits.rate_limit.enabled);
        std::env::remove_var("SHOAL_LISTEN");
        std::env::remove_var("SHOAL_RATE_LIMIT_ENABLED");
    }
}
