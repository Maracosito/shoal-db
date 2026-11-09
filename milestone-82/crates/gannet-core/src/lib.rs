//! # gannet-core
//!
//! Core engine for [Gannet](https://github.com/gannet-db/gannet), an
//! object-storage-native vector + full-text search database.
//!
//! This crate currently provides the foundational **object-storage
//! abstraction** ([`storage::ObjectStore`]) with three backends:
//!
//! - [`storage::MemoryStore`] — in-process, for unit tests.
//! - [`storage::FilesystemStore`] — local filesystem, for development and
//!   single-machine deployments without an object store.
//! - [`storage::S3Store`] — any S3-compatible service (AWS S3, MinIO, R2,
//!   GCS interop, …).
//!
//! All higher layers (WAL, manifests, segments, indexes — see
//! `docs/architecture.md` and `docs/storage-format.md`) are built strictly on
//! top of this trait, so the engine never needs to know which backend it is
//! running against.
//!
//! ## Semantics guaranteed by every backend
//!
//! - `put` is atomic: readers never observe a partially written object.
//! - `put_if_not_exists` is an atomic create-if-absent, used for manifest
//!   commit fencing (compare-and-set on object existence).
//! - `delete` is idempotent: deleting a missing object succeeds.
//! - `list` returns keys in lexicographic order.
//! - Keys are validated against a conservative, portable character set
//!   (see [`storage::validate_key`]).

pub mod error;
pub mod storage;

pub use error::{StorageError, StorageResult};
