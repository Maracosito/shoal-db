//! # reef-types
//!
//! Core data types shared across the ReefDB engine, API server, and tooling.
//!
//! ReefDB is an open-source, object-storage-native vector + full-text search
//! database. Durable state lives entirely in object storage; compute nodes are
//! stateless. This crate defines the *logical* data model:
//!
//! - [`Value`] — a typed attribute value (scalars and homogeneous arrays).
//! - [`DocId`] / [`Document`] — a document with an ID, optional dense vector,
//!   optional sparse vector, and arbitrary metadata attributes.
//! - [`WriteBatch`] / [`WriteOp`] — the row-oriented write interface, with
//!   upsert / patch / delete operations, idempotency keys, and conditional
//!   writes.
//! - [`ColumnarBatch`] — the column-oriented ingestion interface, converted
//!   into the same write operations.
//!
//! These types are deliberately independent of any storage or HTTP concern so
//! the storage engine, API layer, and SDK generators can all depend on them.

pub mod document;
pub mod errors;
pub mod value;
pub mod write;

pub use document::{validate_attr_name, DocId, Document, SparseVector, MAX_DOC_ID_BYTES};
pub use errors::ValidationError;
pub use value::Value;
pub use write::{
    ColumnarBatch, PatchSpec, SparsePatch, UpsertMode, VectorPatch, WriteBatch, WriteCondition,
    WriteOp, MAX_BATCH_OPS, MAX_IDEMPOTENCY_KEY_BYTES,
};
