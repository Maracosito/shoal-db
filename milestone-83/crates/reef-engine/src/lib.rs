//! # reef-engine
//!
//! The Reef storage engine: a durable, object-storage-native write path with
//! WAL, immutable segments, background indexing (folding), compaction,
//! garbage collection, and admin (verify / repair / rebuild) operations.
//!
//! ## Layered design
//!
//! ```text
//!   engine.rs      -- StorageEngine: write/read path, recovery, idempotency,
//!                     preconditions, per-namespace in-memory state
//!   indexer.rs     -- background fold of WAL/memtable into level-0 segments
//!   compaction.rs  -- merges segments into level-1, GC of orphaned objects
//!   admin.rs       -- verify / repair / rebuild from object storage alone
//!   wal.rs         -- one immutable WAL record object per committed batch
//!   manifest.rs    -- versioned manifests; commit point = put-if-absent of
//!                     manifest/<version>; "current" = highest version
//!   segment.rs     -- immutable, checksummed, sorted document segments
//!   memtable.rs    -- in-memory view of WAL records not yet folded
//!   layout.rs      -- object key layout for a namespace
//!   checksum.rs    -- CRC32C framing helpers
//! ```
//!
//! ## Consistency model (single-writer per namespace)
//!
//! * A write is **durable** once its WAL record object has been created with
//!   a conditional (if-absent) put. The WAL record key embeds the LSN, so two
//!   writers can never both believe they committed the same LSN.
//! * A manifest commit is atomic: `manifest/<version>` is created with
//!   put-if-absent; the current manifest is the highest readable version.
//! * Recovery = load current manifest, then replay WAL records with
//!   `lsn > manifest.last_applied_lsn` in LSN order. Patches are resolved
//!   deterministically because the segment set referenced by the manifest is
//!   exactly the state as of `last_applied_lsn`.
//! * Crashes between any two steps of fold/compaction leave either orphaned
//!   objects (cleaned by GC/repair) or un-deleted-but-superseded objects
//!   (ignored by recovery, cleaned by GC). No step can lose committed data.

pub mod admin;
pub mod checksum;
pub mod compaction;
pub mod engine;
pub mod error;
pub mod indexer;
pub mod layout;
pub mod manifest;
pub mod memtable;
pub mod options;
pub mod segment;
pub mod wal;

pub use engine::{
    Engine, IdempotencyEntry, NamespaceStats, Precondition, WriteRequest, WriteResult,
};
pub use error::{EngineError, EngineResult};
pub use options::EngineOptions;

/// Milliseconds since the unix epoch. Used for manifests, WAL records,
/// idempotency retention and GC grace windows.
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
