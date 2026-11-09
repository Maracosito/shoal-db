//! Server configuration.
//!
//! Configuration is loaded from a TOML file (`gannet.toml` by default, or the
//! path given via `--config` / `GANNET_CONFIG`), then selected fields may be
//! overridden via environment variables:
//!
//! | Variable            | Overrides        |
//! |---------------------|------------------|
//! | `GANNET_LISTEN`     | `server.listen`  |
//! | `GANNET_LOG_LEVEL`  | `log.level`      |
//! | `GANNET_LOG_FORMAT` | `log.format`     |
//!
//! Security note: the configuration file deliberately has **no fields for
//! object-storage credentials**. S3-compatible credentials are resolved from
//! the standard environment / credential chain (`AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY`, instance roles, ...). This keeps secrets out of
//! config files, out of `--print-config` output, and out of logs by
//! construction.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Top-level server configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub server: ServerSection,
    pub log: LogSection,
    pub storage: StorageSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSection {
    /// Socket address to bind, e.g. `127.0.0.1:8080` or `0.0.0.0:8080`.
    pub listen: String,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogSection {
    /// Tracing filter directive, e.g. `info` or `info,gannet_core=debug`.
    pub level: String,
    /// Output format: `text` (human-readable) or `json` (one object per line).
    pub format: LogFormat,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "text" | "plain" | "pretty" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            other => Err(format!("unknown log format {other:?} (expected `text` or `json`)")),
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogFormat::Text => f.write_str("text"),
            LogFormat::Json => f.write_str("json"),
        }
    }
}

/// Storage backend selection. This mirrors the backend set implemented by the
/// `gannet-core` storage abstraction (memory, local filesystem, and
/// S3-compatible object storage); the server hands the resolved selection to
/// the engine when the engine is wired in (milestone 2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum StorageSection {
    /// In-memory storage. Non-durable; intended for tests only.
    Memory,
    /// Local filesystem storage rooted at `root`. Single-node durable mode.
    Fs { root: PathBuf },
    /// S3-compatible object storage (AWS S3, MinIO, R2, GCS interop, ...).
    S3 {
        bucket: String,
        /// Key prefix inside the bucket; empty means bucket root.
        #[serde(default)]
        prefix: String,
        /// Region; optional for S3-compatible endpoints such as MinIO.
        #[serde(default)]
        region: Option<String>,
        /// Custom endpoint URL, e.g. `http://minio:9000` for local dev.
        #[serde(default)]
        endpoint: Option<String>,
        /// Use path-style addressing (required by MinIO).
        #[serde(default)]
        force_path_style: bool,
    },
}

impl Default for StorageSection {
    fn default() -> Self {
        StorageSection::Fs {
            root: PathBuf::from("./gannet-data"),
        }
    }
}

impl StorageSection {
    /// Stable machine-readable backend kind for health output and metrics labels.
    pub fn kind(&self) -> &'static str {
        match self {
            StorageSection::Memory => "memory",
            StorageSection::Fs { .. } => "fs",
            StorageSection::S3 { .. } => "s3",
        }
    }

    /// Human-readable, secret-free summary suitable for logging.
    pub fn summary(&self) -> String {
        match self {
            StorageSection::Memory => "memory".to_string(),
            StorageSection::Fs { root } => format!("fs root={}", root.display()),
            StorageSection::S3 {
                bucket,
                prefix,
                region,
                endpoint,
                force_path_style,
            } => format!(
                "s3 bucket={bucket} prefix={prefix:?} region={} endpoint={} path_style={force_path_style}",
                region.as_deref().unwrap_or("<default>"),
                endpoint.as_deref().unwrap_or("<aws>"),
            ),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            StorageSection::Memory => Ok(()),
            StorageSection::Fs { root } => {
                if root.as_os_str().is_empty() {
                    return Err(ConfigError::Invalid(
                        "storage.root must not be empty for the fs backend".into(),
                    ));
                }
                Ok(())
            }
            StorageSection::S3 {
                bucket,
                prefix,
                endpoint,
                ..
            } => {
                if bucket.trim().is_empty() {
                    return Err(ConfigError::Invalid(
                        "storage.bucket must not be empty for the s3 backend".into(),
                    ));
                }
                if prefix.starts_with('/') {
                    return Err(ConfigError::Invalid(
                        "storage.prefix must not start with `/` (keys are relative)".into(),
                    ));
                }
                if let Some(endpoint) = endpoint {
                    url::Url::parse(endpoint).map_err(|e| {
                        ConfigError::Invalid(format!(
                            "storage.endpoint {endpoint:?} is not a valid URL: {e}"
                        ))
                    })?;
                }
                Ok(())
            }
        }
    }
}

impl ServerConfig {
    /// Load configuration.
    ///
    /// * `Some(path)` — read the file at `path`; missing file is an error.
    /// * `None` — read `./gannet.toml` if it exists, else built-in defaults.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        match path {
            Some(p) => Self::load_file(p),
            None => {
                let default = Path::new("gannet.toml");
                if default.exists() {
                    Self::load_file(default)
                } else {
                    Ok(Self::default())
                }
            }
        }
    }

    fn load_file(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Apply environment-variable overrides from the process environment.
    pub fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_from(|key| std::env::var(key).ok());
    }

    /// Apply overrides from an arbitrary lookup function (testable without
    /// mutating process-global environment state).
    pub fn apply_env_overrides_from(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("GANNET_LISTEN") {
            self.server.listen = v;
        }
        if let Some(v) = get("GANNET_LOG_LEVEL") {
            self.log.level = v;
        }
        if let Some(v) = get("GANNET_LOG_FORMAT") {
            match v.parse::<LogFormat>() {
                Ok(f) => self.log.format = f,
                Err(e) => eprintln!("warning: ignoring GANNET_LOG_FORMAT: {e}"),
            }
        }
    }

    /// Validate the assembled configuration. Called after overrides.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.server.listen.parse::<SocketAddr>().map_err(|e| {
            ConfigError::Invalid(format!(
                "server.listen {:?} is not a valid socket address: {e}",
                self.server.listen
            ))
        })?;
        tracing_subscriber::EnvFilter::try_new(&self.log.level).map_err(|e| {
            ConfigError::Invalid(format!(
                "log.level {:?} is not a valid filter directive: {e}",
                self.log.level
            ))
        })?;
        self.storage.validate()
    }

    /// The validated listen address. Panics if `validate` was not called or failed.
    pub fn listen_addr(&self) -> SocketAddr {
        self.server
            .listen
            .parse()
            .expect("listen address validated at startup")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn default_config_is_valid() {
        let cfg = ServerConfig::default();
        cfg.validate().expect("default config must validate");
        assert_eq!(cfg.storage.kind(), "fs");
        assert_eq!(cfg.server.listen, "127.0.0.1:8080");
    }

    #[test]
    fn parses_fs_backend() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            [server]
            listen = "0.0.0.0:9090"
            [log]
            level = "debug"
            format = "json"
            [storage]
            backend = "fs"
            root = "/var/lib/gannet"
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:9090");
        assert_eq!(cfg.log.format, LogFormat::Json);
        assert_eq!(cfg.storage.kind(), "fs");
    }

    #[test]
    fn parses_s3_backend() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            [storage]
            backend = "s3"
            bucket = "gannet-dev"
            prefix = "tenants/dev"
            region = "us-east-1"
            endpoint = "http://localhost:9000"
            force_path_style = true
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.storage.kind(), "s3");
        let summary = cfg.storage.summary();
        assert!(summary.contains("gannet-dev"));
        assert!(summary.contains("http://localhost:9000"));
    }

    #[test]
    fn parses_memory_backend() {
        let cfg: ServerConfig = toml::from_str("[storage]\nbackend = \"memory\"\n").unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.storage.kind(), "memory");
    }

    #[test]
    fn rejects_unknown_top_level_keys() {
        let err = toml::from_str::<ServerConfig>("[serverr]\nlisten = \"x\"\n");
        assert!(err.is_err(), "typoed section must be rejected");
    }

    #[test]
    fn rejects_invalid_listen_address() {
        let mut cfg = ServerConfig::default();
        cfg.server.listen = "not-an-address".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_empty_s3_bucket() {
        let cfg: ServerConfig =
            toml::from_str("[storage]\nbackend = \"s3\"\nbucket = \"\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_absolute_s3_prefix() {
        let cfg: ServerConfig = toml::from_str(
            "[storage]\nbackend = \"s3\"\nbucket = \"b\"\nprefix = \"/abs\"\n",
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_invalid_s3_endpoint() {
        let cfg: ServerConfig = toml::from_str(
            "[storage]\nbackend = \"s3\"\nbucket = \"b\"\nendpoint = \"not a url\"\n",
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn env_overrides_apply() {
        let mut env = HashMap::new();
        env.insert("GANNET_LISTEN".to_string(), "0.0.0.0:7000".to_string());
        env.insert("GANNET_LOG_LEVEL".to_string(), "trace".to_string());
        env.insert("GANNET_LOG_FORMAT".to_string(), "json".to_string());

        let mut cfg = ServerConfig::default();
        cfg.apply_env_overrides_from(|k| env.get(k).cloned());
        cfg.validate().unwrap();

        assert_eq!(cfg.server.listen, "0.0.0.0:7000");
        assert_eq!(cfg.log.level, "trace");
        assert_eq!(cfg.log.format, LogFormat::Json);
    }

    #[test]
    fn summary_never_contains_credentials() {
        // The config schema has no credential fields at all; this test pins
        // that invariant so a future field addition trips review.
        let cfg: ServerConfig = toml::from_str(
            "[storage]\nbackend = \"s3\"\nbucket = \"b\"\nendpoint = \"http://minio:9000\"\n",
        )
        .unwrap();
        let serialized = toml::to_string(&cfg).unwrap();
        for forbidden in ["secret", "password", "token", "access_key"] {
            assert!(
                !serialized.to_lowercase().contains(forbidden),
                "config schema must not carry credentials (found {forbidden:?})"
            );
        }
    }
}
