//! Error types shared across the core engine.

use thiserror::Error;

/// Errors produced by [`crate::storage::ObjectStore`] implementations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The requested object does not exist.
    #[error("object not found: {key}")]
    NotFound { key: String },

    /// A create-if-absent operation found the object already present.
    #[error("object already exists: {key}")]
    AlreadyExists { key: String },

    /// A conditional operation's precondition was not met.
    #[error("precondition failed for {key}: {reason}")]
    PreconditionFailed { key: String, reason: String },

    /// The key (or prefix) is not valid under Gannet's portable key rules.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// An I/O error from the local filesystem backend.
    #[error("i/o error for {key}: {source}")]
    Io {
        key: String,
        #[source]
        source: std::io::Error,
    },

    /// Any other backend-specific failure (network, auth, service error, …).
    #[error("storage backend error for {key}: {message}")]
    Backend { key: String, message: String },
}

impl StorageError {
    /// True if the error indicates the object was not found.
    pub fn is_not_found(&self) -> bool {
        matches!(self, StorageError::NotFound { .. })
    }

    pub(crate) fn backend(key: impl Into<String>, message: impl std::fmt::Display) -> Self {
        StorageError::Backend {
            key: key.into(),
            message: message.to_string(),
        }
    }
}

/// Convenience alias used throughout the storage layer.
pub type StorageResult<T> = Result<T, StorageError>;
