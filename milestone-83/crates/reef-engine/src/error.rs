//! Engine error type shared by the WAL, manifest, segment, engine, indexer,
//! compaction and admin modules.

use thiserror::Error;

/// All errors surfaced by the storage engine.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Underlying object store failure (network, IO, permission, ...).
    #[error("object store error: {0}")]
    Store(#[from] reef_store::StoreError),

    /// The requested entity (namespace, document, manifest version, segment)
    /// does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An entity with the same identity already exists (e.g. creating a
    /// namespace twice).
    #[error("already exists: {0}")]
    AlreadyExists(String),

    /// An optimistic-concurrency conflict: a conditional put lost the race,
    /// usually because another writer process committed first. The caller
    /// should re-read state and retry.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A user-supplied write precondition was not satisfied. The write was
    /// not committed.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),

    /// Data on object storage failed checksum or structural validation.
    /// `reef-admin verify` / `repair` / `rebuild` are the recovery tools.
    #[error("corruption detected: {0}")]
    Corruption(String),

    /// The request itself is malformed (empty batch, bad namespace name, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// (De)serialization failure for manifests, WAL records or ledgers.
    #[error("serialization error: {0}")]
    Serde(String),

    /// Local IO error (only used by filesystem-adjacent helpers).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<serde_json::Error> for EngineError {
    fn from(e: serde_json::Error) -> Self {
        EngineError::Serde(e.to_string())
    }
}

impl EngineError {
    /// True if this error indicates a missing entity.
    pub fn is_not_found(&self) -> bool {
        matches!(self, EngineError::NotFound(_))
            || matches!(self, EngineError::Store(e) if e.is_not_found())
    }

    /// True if this error is a retryable optimistic-concurrency conflict.
    pub fn is_conflict(&self) -> bool {
        matches!(self, EngineError::Conflict(_))
    }

    /// True if this error indicates on-storage corruption.
    pub fn is_corruption(&self) -> bool {
        matches!(self, EngineError::Corruption(_))
    }
}

/// Result alias used across the engine crate.
pub type EngineResult<T> = std::result::Result<T, EngineError>;
