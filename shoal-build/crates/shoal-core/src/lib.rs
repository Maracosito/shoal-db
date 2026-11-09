//! # shoal-core
//!
//! Core engine for Shoal, an object-storage-native vector + full-text search
//! database. Durable state lives entirely in object storage (S3-compatible,
//! local filesystem, or in-memory for tests). Compute is stateless: a node can
//! be restarted at any time and reconstructs namespace state from the
//! manifest + write-ahead log.
//!
//! Modules:
//! - [`storage`]: object storage abstraction (memory / local FS / S3) with I/O stats.
//! - [`layout`]: the on-storage key layout for registries, manifests, WAL and segments.
//! - [`wal`]: write-ahead log entries (one object per write batch, CRC framed).
//! - [`segment`]: immutable queryable segments with embedded inverted text index
//!   and attribute range summaries.
//! - [`manifest`]: per-namespace manifest (segment refs, flushed WAL position,
//!   branch parent, idempotency window).
//! - [`registry`]: org/project/namespace registry and segment reference counts
//!   used for copy-on-write branching.
//! - [`loader`]: pluggable segment loading (the cache crate implements a tiered loader).
//! - [`engine`]: the namespace engine: writes, replay, flush, compaction,
//!   branching/copying with refcounted shared segments, and baseline query
//!   execution (exact kNN, BM25, hybrid fusion, filters).

pub mod engine;
pub mod error;
pub mod filter;
pub mod framing;
pub mod layout;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod segment;
pub mod storage;
pub mod types;
pub mod wal;

pub use error::{Result, ShoalError};
