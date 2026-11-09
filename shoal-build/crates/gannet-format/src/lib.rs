//! # gannet-format
//!
//! The single source of truth, in code, for Gannet's persistent formats and
//! wire-adjacent value types. This crate is the executable companion to two
//! design documents in this repository:
//!
//! * `docs/storage-format.md` — the object-storage layout (namespace
//!   manifests, manifest pointer files, WAL files, immutable segments, and
//!   branch references) and the format versioning rules.
//! * `api/openapi.yaml` — the HTTP API, whose document, attribute, and filter
//!   schemas are mirrored here so the engine and API server share one
//!   definition.
//!
//! ## Design principles
//!
//! 1. **Everything durable is versioned.** Every manifest and pointer file
//!    carries a `format` field of the shape `"MAJOR.MINOR"`. Readers accept
//!    any file with a matching major version; a newer minor version is
//!    read-compatible (unknown fields are preserved, not dropped — see the
//!    `extra` flatten maps). A different major version is rejected loudly
//!    rather than misinterpreted. See [`version`].
//!
//! 2. **Keys sort the way time flows.** WAL sequence numbers and manifest
//!    generations are zero-padded to 20 digits so that lexicographic order
//!    over object keys equals numeric order, which lets a `list` call on any
//!    S3-compatible store return history in commit order. See [`keys`].
//!
//! 3. **Ownership is encoded in the key.** A branch's manifest may reference
//!    segments whose keys live under the *parent* namespace prefix. Garbage
//!    collection and branch deletion only ever delete keys under a
//!    namespace's own prefix, which is what makes copy-on-write branching
//!    safe. See [`keys::Layout::key_in_namespace`] and
//!    [`manifest::SegmentRef`].
//!
//! These types are deliberately free of any I/O: `gannet-core` performs
//! storage operations through its backend trait and uses this crate purely
//! for encoding, decoding, and validation.

pub mod document;
pub mod filter;
pub mod ident;
pub mod integrity;
pub mod keys;
pub mod manifest;
pub mod version;

pub use document::{AttributeValue, Document, DocumentId, SparseVector};
pub use filter::Filter;
pub use keys::Layout;
pub use manifest::{
    BranchParent, ManifestPointer, NamespaceManifest, NamespaceStats, SegmentKind, SegmentRef,
    WalFileRef,
};
pub use version::{Compatibility, FormatVersion, CURRENT_FORMAT};
