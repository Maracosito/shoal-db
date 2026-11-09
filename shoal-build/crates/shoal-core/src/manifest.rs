//! Namespace manifest: the single mutable metadata object per namespace.
//!
//! The manifest is the *only* mutable object in a namespace's storage layout.
//! Everything else (WAL batches, segments) is immutable once written. A
//! namespace's entire logical state is therefore: `manifest + the immutable
//! objects it references`.
//!
//! # Milestone 4 revision (format v2)
//!
//! To support copy-on-write namespace branching, segment and WAL references
//! now carry **absolute object keys** rather than namespace-relative file
//! names. A branch manifest can thus reference immutable objects that
//! physically live under the *source* namespace's prefix. Reference counting
//! (maintained by the server's namespace engine in a per-project refcount
//! table) ensures shared objects are only deleted when no manifest references
//! them anymore.
//!
//! Additional v2 fields:
//! - `parent`: the namespace path this namespace was branched from (None for
//!   roots and plain copies).
//! - `labels`: free-form user metadata on the namespace.
//! - `vector_dim`: pinned dense-vector dimensionality, set on first vector
//!   write and enforced on subsequent writes and queries.
//!
//! All v2 fields are `#[serde(default)]` so v1 manifests written by earlier
//! milestones still deserialize (their refs must be rewritten to absolute
//! keys by the `shoal repair` flow before branching is used on them).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::DistanceMetric;

/// Current manifest format version.
pub const MANIFEST_FORMAT: u32 = 2;

/// Reference to one immutable segment object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// Opaque unique segment id (used for cache keys and debugging).
    pub id: String,
    /// Absolute object-storage key of the segment object.
    pub key: String,
    /// Number of documents stored in the segment.
    pub doc_count: u64,
    /// Size of the segment object in bytes.
    pub size_bytes: u64,
    /// Creation timestamp (epoch milliseconds).
    #[serde(default)]
    pub created_at_ms: u64,
}

/// Reference to one immutable WAL batch object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalRef {
    /// Absolute object-storage key of the WAL object.
    pub key: String,
    /// Sequence number of the first record in the batch.
    pub first_seq: u64,
    /// Sequence number of the last record in the batch.
    pub last_seq: u64,
    /// Number of records in the batch.
    pub record_count: u64,
    /// Size of the WAL object in bytes.
    pub size_bytes: u64,
}

/// The per-namespace manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest format version.
    #[serde(default = "default_format")]
    pub format: u32,
    /// Full namespace path, `org/project/namespace`.
    pub namespace: String,
    /// Monotonic version, incremented on every committed change. Used for
    /// optimistic concurrency (`if_version` conditional writes) and for
    /// cache invalidation of materialized state.
    pub version: u64,
    /// Next WAL sequence number to assign.
    pub next_seq: u64,
    /// Distance metric used for dense-vector scoring in this namespace.
    pub distance_metric: DistanceMetric,
    /// Dense vector dimensionality, fixed on first vector write.
    #[serde(default)]
    pub vector_dim: Option<u32>,
    /// Namespace path this namespace was branched from, if any.
    #[serde(default)]
    pub parent: Option<String>,
    /// Free-form user labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Immutable segment objects, oldest first. Later objects win on id
    /// conflicts during materialization.
    #[serde(default)]
    pub segments: Vec<SegmentRef>,
    /// Immutable WAL batch objects not yet folded into a segment, in
    /// ascending sequence order. Applied after all segments.
    #[serde(default)]
    pub wal: Vec<WalRef>,
    /// Approximate live document count. Exact after each compaction;
    /// adjusted heuristically by intermediate writes.
    #[serde(default)]
    pub doc_count: u64,
    /// Creation timestamp (epoch milliseconds).
    #[serde(default)]
    pub created_at_ms: u64,
    /// Last update timestamp (epoch milliseconds).
    #[serde(default)]
    pub updated_at_ms: u64,
}

fn default_format() -> u32 {
    MANIFEST_FORMAT
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Manifest {
    /// Create a fresh, empty manifest for a new namespace.
    pub fn new(namespace: &str, distance_metric: DistanceMetric) -> Self {
        let now = now_ms();
        Manifest {
            format: MANIFEST_FORMAT,
            namespace: namespace.to_string(),
            version: 1,
            next_seq: 0,
            distance_metric,
            vector_dim: None,
            parent: None,
            labels: BTreeMap::new(),
            segments: Vec::new(),
            wal: Vec::new(),
            doc_count: 0,
            created_at_ms: now,
            updated_at_ms: now,
        }
    }

    /// Serialize to pretty JSON bytes (manifests are small and human
    /// debuggability matters more than density here).
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Deserialize from JSON bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// All immutable object keys referenced by this manifest (segments then
    /// WAL objects). These are exactly the keys that participate in
    /// reference counting.
    pub fn object_keys(&self) -> Vec<String> {
        self.segments
            .iter()
            .map(|s| s.key.clone())
            .chain(self.wal.iter().map(|w| w.key.clone()))
            .collect()
    }

    /// Total referenced bytes (segments + WAL).
    pub fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.size_bytes).sum::<u64>()
            + self.wal.iter().map(|w| w.size_bytes).sum::<u64>()
    }

    /// Total WAL records not yet compacted into segments.
    pub fn wal_records(&self) -> u64 {
        self.wal.iter().map(|w| w.record_count).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut m = Manifest::new("acme/search/docs", DistanceMetric::Cosine);
        m.vector_dim = Some(384);
        m.parent = Some("acme/search/base".to_string());
        m.labels.insert("env".into(), "dev".into());
        m.segments.push(SegmentRef {
            id: "seg1".into(),
            key: "orgs/acme/projects/search/namespaces/docs/segments/seg1.seg".into(),
            doc_count: 10,
            size_bytes: 1234,
            created_at_ms: 1,
        });
        m.wal.push(WalRef {
            key: "orgs/acme/projects/search/namespaces/docs/wal/00000000000000000000-x.wal".into(),
            first_seq: 0,
            last_seq: 4,
            record_count: 5,
            size_bytes: 99,
        });
        let bytes = m.to_bytes().unwrap();
        let back = Manifest::from_bytes(&bytes).unwrap();
        assert_eq!(back.namespace, m.namespace);
        assert_eq!(back.version, 1);
        assert_eq!(back.vector_dim, Some(384));
        assert_eq!(back.parent.as_deref(), Some("acme/search/base"));
        assert_eq!(back.segments, m.segments);
        assert_eq!(back.wal, m.wal);
        assert_eq!(back.total_bytes(), 1234 + 99);
        assert_eq!(back.wal_records(), 5);
    }

    #[test]
    fn object_keys_in_order() {
        let mut m = Manifest::new("a/b/c", DistanceMetric::Cosine);
        m.segments.push(SegmentRef {
            id: "s".into(),
            key: "k1".into(),
            doc_count: 0,
            size_bytes: 0,
            created_at_ms: 0,
        });
        m.wal.push(WalRef {
            key: "k2".into(),
            first_seq: 0,
            last_seq: 0,
            record_count: 1,
            size_bytes: 1,
        });
        assert_eq!(m.object_keys(), vec!["k1".to_string(), "k2".to_string()]);
    }

    #[test]
    fn v2_fields_default_when_absent() {
        // A minimal manifest body (as a v1 writer would have produced,
        // modulo key shape) must still deserialize.
        let body = serde_json::json!({
            "namespace": "a/b/c",
            "version": 3,
            "next_seq": 17,
            "distance_metric": Manifest::new("x", DistanceMetric::Cosine).distance_metric,
        });
        let m: Manifest = serde_json::from_value(body).unwrap();
        assert_eq!(m.version, 3);
        assert!(m.parent.is_none());
        assert!(m.vector_dim.is_none());
        assert!(m.labels.is_empty());
        assert!(m.segments.is_empty());
        assert!(m.wal.is_empty());
    }
}
