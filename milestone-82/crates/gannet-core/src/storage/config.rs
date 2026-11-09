//! Declarative storage configuration and the backend factory.
//!
//! Storage configuration is deserialized from the server/CLI config file
//! (YAML or JSON) using an internally tagged enum:
//!
//! ```yaml
//! # local filesystem
//! storage:
//!   backend: filesystem
//!   root: ./data
//!
//! # MinIO / S3
//! storage:
//!   backend: s3
//!   bucket: gannet
//!   endpoint: http://localhost:9000
//!   access_key_id: minioadmin
//!   secret_access_key: minioadmin
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::StorageResult;

use super::{DynObjectStore, FilesystemStore, MemoryStore, S3Config, S3Store};

/// Which object-storage backend to use, plus its settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum StorageConfig {
    /// In-process memory. Only for tests — nothing is durable.
    Memory,
    /// Local filesystem rooted at `root`.
    Filesystem { root: PathBuf },
    /// S3-compatible object storage (AWS S3, MinIO, …).
    S3(S3Config),
}

/// Construct an [`super::ObjectStore`] from configuration.
pub async fn open_store(config: &StorageConfig) -> StorageResult<DynObjectStore> {
    match config {
        StorageConfig::Memory => Ok(Arc::new(MemoryStore::new())),
        StorageConfig::Filesystem { root } => {
            Ok(Arc::new(FilesystemStore::new(root.clone())?))
        }
        StorageConfig::S3(cfg) => Ok(Arc::new(S3Store::from_config(cfg).await?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_filesystem() {
        let json = r#"{"backend":"filesystem","root":"/var/lib/gannet"}"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match &cfg {
            StorageConfig::Filesystem { root } => {
                assert_eq!(root, &PathBuf::from("/var/lib/gannet"))
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let back = serde_json::to_string(&cfg).unwrap();
        let cfg2: StorageConfig = serde_json::from_str(&back).unwrap();
        assert!(matches!(cfg2, StorageConfig::Filesystem { .. }));
    }

    #[test]
    fn config_roundtrip_s3() {
        let json = r#"{
            "backend": "s3",
            "bucket": "gannet",
            "endpoint": "http://localhost:9000",
            "access_key_id": "minioadmin",
            "secret_access_key": "minioadmin"
        }"#;
        let cfg: StorageConfig = serde_json::from_str(json).unwrap();
        match &cfg {
            StorageConfig::S3(s3) => {
                assert_eq!(s3.bucket, "gannet");
                assert_eq!(s3.endpoint.as_deref(), Some("http://localhost:9000"));
                assert!(s3.force_path_style.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn config_memory() {
        let cfg: StorageConfig = serde_json::from_str(r#"{"backend":"memory"}"#).unwrap();
        assert!(matches!(cfg, StorageConfig::Memory));
    }
}
