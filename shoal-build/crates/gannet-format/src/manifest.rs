//! Namespace manifests and the manifest pointer file.
//!
//! A **manifest** is an immutable JSON object describing one *generation* of
//! a namespace: the set of WAL files not yet folded into segments, the set
//! of immutable segments, optional branch parentage, and rolled-up stats.
//! Manifests are written to `manifests/{generation:020}.json` and are never
//! modified.
//!
//! The **pointer file** (`CURRENT`) is the only mutable object per
//! namespace. It names the latest manifest. Commits proceed as:
//!
//! 1. write new WAL/segment objects (immutable, content-addressed names),
//! 2. write the new manifest at generation `N + 1` (put-if-absent where the
//!    backend supports it — colliding writers lose deterministically),
//! 3. swap `CURRENT` to point at generation `N + 1`.
//!
//! A crash between any of these steps leaves only unreferenced garbage,
//! never a corrupt namespace; recovery is "read CURRENT, read manifest,
//! ignore everything not referenced". See `docs/architecture.md` for the
//! full write-path discussion.
//!
//! ## Forward compatibility
//!
//! Both structures carry a [`FormatVersion`] and an `extra` flatten map.
//! Fields added in future *minor* versions land in `extra` when read by this
//! build and are re-serialized verbatim, satisfying the "preserve unknown
//! fields" rule from `docs/storage-format.md`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::version::{Compatibility, FormatVersion, CURRENT_FORMAT};

/// Errors arising from manifest encoding/decoding.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to encode/decode manifest JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("file format version {found} is not readable by this build (writes {current})")]
    IncompatibleVersion {
        found: FormatVersion,
        current: FormatVersion,
    },
}

/// Reference to one immutable WAL file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WalFileRef {
    /// Monotonic WAL sequence number within the namespace.
    pub seq: u64,
    /// Full object key of the WAL file.
    pub key: String,
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file contents.
    pub sha256: String,
    /// Number of records (upserts/patches/deletes) in the file.
    pub record_count: u64,
}

/// The kind of an immutable segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    /// Row data: document payloads addressable by ID.
    Docs,
    /// IVF/centroid-partitioned dense-vector index.
    VectorIvf,
    /// Inverted index + BM25 statistics for full-text search.
    FullText,
    /// Attribute (metadata filter) index.
    Attribute,
}

/// Reference to one file of an immutable segment.
///
/// `key` is a *full object key* and may point under a different namespace's
/// prefix when this manifest belongs to a branch (copy-on-write sharing).
/// Whether the segment is owned or shared is derived from the key prefix —
/// see `keys::Layout::key_in_namespace` — and is deliberately not duplicated
/// as a flag that could drift out of sync.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// Stable segment identifier (unique within its owning namespace).
    pub id: String,
    pub kind: SegmentKind,
    /// Full object key of the segment file.
    pub key: String,
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file contents.
    pub sha256: String,
    /// Number of live documents covered by this segment.
    pub doc_count: u64,
    /// Compaction level (0 = freshly flushed; higher = more compacted).
    #[serde(default)]
    pub level: u8,
}

/// Branch parentage: which namespace and generation this branch forked from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchParent {
    pub namespace_id: String,
    /// The parent generation whose manifest was snapshotted at fork time.
    pub generation: u64,
}

/// Rolled-up statistics carried in each manifest.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamespaceStats {
    #[serde(default)]
    pub approx_doc_count: u64,
    #[serde(default)]
    pub approx_deleted_count: u64,
    #[serde(default)]
    pub total_bytes: u64,
}

/// One immutable generation of a namespace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamespaceManifest {
    pub format: FormatVersion,
    pub namespace_id: String,
    /// Monotonic generation; the pointer file names the live one.
    pub generation: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<BranchParent>,
    /// WAL files not yet folded into segments, in ascending `seq` order.
    #[serde(default)]
    pub wal: Vec<WalFileRef>,
    /// Live immutable segments.
    #[serde(default)]
    pub segments: Vec<SegmentRef>,
    #[serde(default)]
    pub stats: NamespaceStats,
    /// Unknown fields from newer minor versions, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl NamespaceManifest {
    /// A fresh generation-0 manifest for a new, empty namespace.
    pub fn new(namespace_id: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            format: CURRENT_FORMAT,
            namespace_id: namespace_id.into(),
            generation: 0,
            created_at: now,
            updated_at: now,
            parent: None,
            wal: Vec::new(),
            segments: Vec::new(),
            stats: NamespaceStats::default(),
            extra: BTreeMap::new(),
        }
    }

    pub fn next_generation(&self) -> u64 {
        self.generation + 1
    }

    /// Serialize to canonical (pretty, UTF-8) JSON.
    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Parse and enforce the format-version compatibility rules.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: NamespaceManifest = serde_json::from_slice(bytes)?;
        if manifest.format.compatibility_with_current() == Compatibility::Incompatible {
            return Err(ManifestError::IncompatibleVersion {
                found: manifest.format,
                current: CURRENT_FORMAT,
            });
        }
        Ok(manifest)
    }
}

/// The single mutable object per namespace: names the live manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestPointer {
    pub format: FormatVersion,
    pub generation: u64,
    /// Full object key of the manifest this pointer designates.
    pub manifest_key: String,
    /// Unknown fields from newer minor versions, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ManifestPointer {
    pub fn new(generation: u64, manifest_key: impl Into<String>) -> Self {
        Self {
            format: CURRENT_FORMAT,
            generation,
            manifest_key: manifest_key.into(),
            extra: BTreeMap::new(),
        }
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, ManifestError> {
        let pointer: ManifestPointer = serde_json::from_slice(bytes)?;
        if pointer.format.compatibility_with_current() == Compatibility::Incompatible {
            return Err(ManifestError::IncompatibleVersion {
                found: pointer.format,
                current: CURRENT_FORMAT,
            });
        }
        Ok(pointer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Layout;

    #[test]
    fn new_manifest_defaults() {
        let m = NamespaceManifest::new("docs");
        assert_eq!(m.format, CURRENT_FORMAT);
        assert_eq!(m.generation, 0);
        assert_eq!(m.next_generation(), 1);
        assert!(m.wal.is_empty());
        assert!(m.segments.is_empty());
        assert!(m.parent.is_none());
    }

    #[test]
    fn manifest_round_trip() {
        let layout = Layout::default();
        let mut m = NamespaceManifest::new("docs");
        m.generation = 3;
        m.wal.push(WalFileRef {
            seq: 7,
            key: layout.wal_key("docs", 7),
            size_bytes: 1024,
            sha256: "ab".repeat(32),
            record_count: 12,
        });
        m.segments.push(SegmentRef {
            id: "seg-0001".into(),
            kind: SegmentKind::VectorIvf,
            key: layout.segment_key("docs", "seg-0001", "vectors.ivf"),
            size_bytes: 4096,
            sha256: "cd".repeat(32),
            doc_count: 100,
            level: 1,
        });
        m.parent = Some(BranchParent {
            namespace_id: "base".into(),
            generation: 9,
        });
        m.stats.approx_doc_count = 100;

        let bytes = m.to_json().unwrap();
        let back = NamespaceManifest::from_json(&bytes).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn pointer_round_trip() {
        let p = ManifestPointer::new(5, Layout::default().manifest_key("docs", 5));
        let bytes = p.to_json().unwrap();
        let back = ManifestPointer::from_json(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn segment_kind_wire_names() {
        let json = serde_json::to_string(&SegmentKind::VectorIvf).unwrap();
        assert_eq!(json, "\"vector_ivf\"");
        let json = serde_json::to_string(&SegmentKind::FullText).unwrap();
        assert_eq!(json, "\"full_text\"");
    }
}
