//! Error type shared by every component of the query engine.

use thiserror::Error;

/// All errors produced by the query engine.
#[derive(Debug, Error)]
pub enum QueryError {
    /// A vector did not have the dimensionality the index/query expects.
    #[error("dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    /// A caller passed an argument that can never be valid (empty input,
    /// mismatched lengths, unsorted sparse indices, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A serialized index blob is malformed (bad magic, truncated,
    /// inconsistent offsets, unknown version, ...).
    #[error("invalid index format: {0}")]
    InvalidIndexFormat(String),

    /// A serialized index blob failed checksum verification. This usually
    /// indicates a corrupted or partially-written object.
    #[error("checksum mismatch in {context}: stored {stored:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        context: &'static str,
        stored: u32,
        computed: u32,
    },

    /// The query itself is invalid (e.g. no rank criterion, top_k of zero,
    /// hybrid weights that do not make sense).
    #[error("invalid query: {0}")]
    InvalidQuery(String),

    /// A filter or projection referenced an attribute the engine knows
    /// nothing about (only raised in strict-schema mode).
    #[error("unknown attribute: {0}")]
    UnknownAttribute(String),

    /// A filter compared an attribute against an operand of an
    /// incompatible type.
    #[error("type mismatch in filter on attribute '{attribute}': {detail}")]
    FilterTypeMismatch { attribute: String, detail: String },

    /// I/O error surfaced while reading index data.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Invariant violation inside the engine; always a bug.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience result alias used across the crate.
pub type Result<T> = std::result::Result<T, QueryError>;
