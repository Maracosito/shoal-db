//! Validation errors for the logical data model.

use thiserror::Error;

/// Errors produced while validating user-supplied data (documents, batches,
/// attribute values). These map to HTTP 400 responses at the API layer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("invalid document id: {0}")]
    InvalidId(String),

    #[error("invalid attribute name {name:?}: {reason}")]
    InvalidAttrName { name: String, reason: String },

    #[error("invalid vector: {0}")]
    InvalidVector(String),

    #[error("invalid sparse vector: {0}")]
    InvalidSparse(String),

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("unsupported JSON value: {0}")]
    UnsupportedJson(String),

    #[error("column length mismatch: {0}")]
    LengthMismatch(String),

    #[error("write batch is empty")]
    EmptyBatch,

    #[error("write batch too large: {got} ops (max {max})")]
    BatchTooLarge { got: usize, max: usize },

    #[error("invalid idempotency key: {0}")]
    InvalidIdempotencyKey(String),

    #[error("invalid patch: {0}")]
    InvalidPatch(String),
}
