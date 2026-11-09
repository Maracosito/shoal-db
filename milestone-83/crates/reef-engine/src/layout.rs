//! Object key layout for a namespace.
//!
//! All sequence numbers are zero-padded to 20 digits so lexicographic key
//! order equals numeric order — `list` on a prefix returns WAL objects and
//! manifests already sorted by sequence.

use crate::error::{EngineError, EngineResult};

/// Width of zero-padded sequence numbers in object keys (fits all of u64).
pub const SEQ_WIDTH: usize = 20;

pub const WAL_SUFFIX: &str = ".wal";
pub const MANIFEST_SUFFIX: &str = ".manifest.json";
pub const SEGMENT_SUFFIX: &str = ".seg";

/// Format a sequence number as a fixed-width, lexicographically sortable
/// string.
pub fn seq_str(seq: u64) -> String {
    format!("{seq:020}")
}

/// Key builder for a single namespace rooted at some prefix (for example
/// `ns/<namespace-id>` or, for branches, the branch's own root — branching
/// is a later milestone but the layout already isolates each namespace under
/// one root so segment objects can be shared by reference).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespacePaths {
    root: String,
}

impl NamespacePaths {
    pub fn new(root: impl Into<String>) -> Self {
        let mut root: String = root.into();
        while root.ends_with('/') {
            root.pop();
        }
        NamespacePaths { root }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// The CURRENT hint object: holds the latest committed manifest sequence
    /// as ASCII digits. It is a *hint* only — written best-effort after a
    /// manifest commit. Correctness never depends on it; loaders verify by
    /// probing forward and fall back to listing the manifest prefix.
    pub fn current(&self) -> String {
        format!("{}/CURRENT", self.root)
    }

    pub fn manifest_prefix(&self) -> String {
        format!("{}/manifests/", self.root)
    }

    pub fn manifest(&self, seq: u64) -> String {
        format!("{}/manifests/{}{}", self.root, seq_str(seq), MANIFEST_SUFFIX)
    }

    pub fn wal_prefix(&self) -> String {
        format!("{}/wal/", self.root)
    }

    pub fn wal(&self, seq: u64) -> String {
        format!("{}/wal/{}{}", self.root, seq_str(seq), WAL_SUFFIX)
    }

    pub fn segment_prefix(&self) -> String {
        format!("{}/segments/", self.root)
    }

    /// Segment objects are named by ULID, not sequence: they are created
    /// speculatively by indexing/compaction before the manifest references
    /// them, and ULIDs guarantee a crashed attempt can never collide with a
    /// retry.
    pub fn segment(&self, segment_id: &str) -> String {
        format!("{}/segments/{}{}", self.root, segment_id, SEGMENT_SUFFIX)
    }

    pub fn quarantine_prefix(&self) -> String {
        format!("{}/quarantine/", self.root)
    }

    /// Where `repair` moves a corrupt object, preserving its original name.
    pub fn quarantine(&self, original_key: &str) -> String {
        let name = original_key.rsplit('/').next().unwrap_or(original_key);
        format!("{}/quarantine/{}", self.root, name)
    }

    /// Parse the WAL sequence out of a full WAL key under this namespace.
    pub fn parse_wal_seq(&self, key: &str) -> EngineResult<u64> {
        parse_seq(key, &self.wal_prefix(), WAL_SUFFIX)
    }

    /// Parse the manifest sequence out of a full manifest key.
    pub fn parse_manifest_seq(&self, key: &str) -> EngineResult<u64> {
        parse_seq(key, &self.manifest_prefix(), MANIFEST_SUFFIX)
    }

    /// Parse the segment ID out of a full segment key.
    pub fn parse_segment_id<'a>(&self, key: &'a str) -> EngineResult<&'a str> {
        let prefix = self.segment_prefix();
        let rest = key.strip_prefix(prefix.as_str()).ok_or_else(|| {
            EngineError::InvalidArgument(format!("key {key:?} is not under {prefix:?}"))
        })?;
        rest.strip_suffix(SEGMENT_SUFFIX).ok_or_else(|| {
            EngineError::InvalidArgument(format!(
                "key {key:?} does not end with {SEGMENT_SUFFIX:?}"
            ))
        })
    }
}

fn parse_seq(key: &str, prefix: &str, suffix: &str) -> EngineResult<u64> {
    let rest = key.strip_prefix(prefix).ok_or_else(|| {
        EngineError::InvalidArgument(format!("key {key:?} is not under {prefix:?}"))
    })?;
    let digits = rest.strip_suffix(suffix).ok_or_else(|| {
        EngineError::InvalidArgument(format!("key {key:?} does not end with {suffix:?}"))
    })?;
    if digits.len() != SEQ_WIDTH || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(EngineError::InvalidArgument(format!(
            "key {key:?} has malformed sequence {digits:?}"
        )));
    }
    digits
        .parse::<u64>()
        .map_err(|e| EngineError::InvalidArgument(format!("key {key:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_str_is_sortable() {
        assert_eq!(seq_str(7), "00000000000000000007");
        assert_eq!(seq_str(u64::MAX), "18446744073709551615");
        assert!(seq_str(9) < seq_str(10));
        assert!(seq_str(99) < seq_str(100));
    }

    #[test]
    fn key_construction_and_parsing() {
        let p = NamespacePaths::new("ns/abc123/");
        assert_eq!(p.root(), "ns/abc123");
        assert_eq!(p.current(), "ns/abc123/CURRENT");
        assert_eq!(
            p.wal(7),
            "ns/abc123/wal/00000000000000000007.wal"
        );
        assert_eq!(
            p.manifest(3),
            "ns/abc123/manifests/00000000000000000003.manifest.json"
        );
        assert_eq!(
            p.segment("01HZX5"),
            "ns/abc123/segments/01HZX5.seg"
        );

        assert_eq!(p.parse_wal_seq(&p.wal(7)).unwrap(), 7);
        assert_eq!(p.parse_manifest_seq(&p.manifest(3)).unwrap(), 3);
        assert_eq!(p.parse_segment_id(&p.segment("01HZX5")).unwrap(), "01HZX5");
    }

    #[test]
    fn parse_rejects_malformed() {
        let p = NamespacePaths::new("ns/abc");
        assert!(p.parse_wal_seq("ns/other/wal/00000000000000000001.wal").is_err());
        assert!(p.parse_wal_seq("ns/abc/wal/1.wal").is_err());
        assert!(p.parse_wal_seq("ns/abc/wal/0000000000000000000x.wal").is_err());
        assert!(p.parse_wal_seq("ns/abc/wal/00000000000000000001.seg").is_err());
        assert!(p
            .parse_manifest_seq("ns/abc/manifests/00000000000000000001.json")
            .is_err());
    }

    #[test]
    fn quarantine_preserves_name() {
        let p = NamespacePaths::new("ns/abc");
        assert_eq!(
            p.quarantine("ns/abc/wal/00000000000000000004.wal"),
            "ns/abc/quarantine/00000000000000000004.wal"
        );
    }
}
