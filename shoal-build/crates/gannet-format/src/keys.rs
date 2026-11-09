//! Canonical object-storage key layout, as specified in
//! `docs/storage-format.md`.
//!
//! ```text
//! {root}/namespaces/{namespace_id}/CURRENT                      <- pointer to latest manifest
//! {root}/namespaces/{namespace_id}/manifests/{gen:020}.json     <- immutable manifest, one per generation
//! {root}/namespaces/{namespace_id}/wal/{seq:020}.wal            <- immutable WAL files
//! {root}/namespaces/{namespace_id}/segments/{segment_id}/{file} <- immutable segment files
//! ```
//!
//! Sequence numbers and generations are zero-padded to 20 digits so that the
//! lexicographic order returned by `list` on any S3-compatible store equals
//! numeric (commit) order, and so that every `u64` fits without truncation.
//!
//! **Ownership rule:** a namespace may only ever *delete* keys under its own
//! prefix. Branch manifests reference segment keys under their parent's
//! prefix; those references are read-only by construction, which is what
//! makes copy-on-write branching and safe branch deletion possible.

/// File name of the per-namespace manifest pointer object.
pub const CURRENT_POINTER_FILE: &str = "CURRENT";

/// Width used for zero-padding sequence numbers and generations.
pub const SEQ_PAD_WIDTH: usize = 20;

/// Key builder for a particular root prefix (e.g. `""` for the bucket root,
/// or `"tenant-a"` for multi-tenant buckets).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    root: String,
}

impl Layout {
    /// Create a layout rooted at `root`. Leading/trailing slashes are
    /// normalized away; an empty root means the bucket/directory root.
    pub fn new(root: impl Into<String>) -> Self {
        let root = root.into();
        Self {
            root: root.trim_matches('/').to_string(),
        }
    }

    fn prefix(&self) -> String {
        if self.root.is_empty() {
            String::new()
        } else {
            format!("{}/", self.root)
        }
    }

    /// Prefix under which *all* keys for a namespace live.
    pub fn namespace_prefix(&self, namespace_id: &str) -> String {
        format!("{}namespaces/{}/", self.prefix(), namespace_id)
    }

    /// Key of the manifest pointer file (`CURRENT`).
    pub fn current_pointer_key(&self, namespace_id: &str) -> String {
        format!(
            "{}{}",
            self.namespace_prefix(namespace_id),
            CURRENT_POINTER_FILE
        )
    }

    /// Key of the immutable manifest for a given generation.
    pub fn manifest_key(&self, namespace_id: &str, generation: u64) -> String {
        format!(
            "{}manifests/{:020}.json",
            self.namespace_prefix(namespace_id),
            generation
        )
    }

    /// Key of an immutable WAL file with the given sequence number.
    pub fn wal_key(&self, namespace_id: &str, seq: u64) -> String {
        format!("{}wal/{:020}.wal", self.namespace_prefix(namespace_id), seq)
    }

    /// Prefix under which all WAL files of a namespace live.
    pub fn wal_prefix(&self, namespace_id: &str) -> String {
        format!("{}wal/", self.namespace_prefix(namespace_id))
    }

    /// Key of a file inside an immutable segment directory.
    pub fn segment_key(&self, namespace_id: &str, segment_id: &str, file_name: &str) -> String {
        format!(
            "{}segments/{}/{}",
            self.namespace_prefix(namespace_id),
            segment_id,
            file_name
        )
    }

    /// Whether `key` is owned by (lives under the prefix of) `namespace_id`.
    /// Garbage collection and branch deletion must only delete owned keys.
    pub fn key_in_namespace(&self, namespace_id: &str, key: &str) -> bool {
        key.starts_with(&self.namespace_prefix(namespace_id))
    }

    /// Extract the sequence number from a WAL key, if it is one.
    pub fn wal_seq_from_key(key: &str) -> Option<u64> {
        let mut parts = key.rsplit('/');
        let file = parts.next()?;
        let dir = parts.next()?;
        if dir != "wal" {
            return None;
        }
        file.strip_suffix(".wal")?.parse().ok()
    }

    /// Extract the generation from a manifest key, if it is one.
    pub fn manifest_generation_from_key(key: &str) -> Option<u64> {
        let mut parts = key.rsplit('/');
        let file = parts.next()?;
        let dir = parts.next()?;
        if dir != "manifests" {
            return None;
        }
        file.strip_suffix(".json")?.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_normalization() {
        assert_eq!(Layout::new(""), Layout::default());
        assert_eq!(Layout::new("/tenant-a/"), Layout::new("tenant-a"));
        assert_eq!(
            Layout::new("tenant-a").namespace_prefix("docs"),
            "tenant-a/namespaces/docs/"
        );
        assert_eq!(Layout::default().namespace_prefix("docs"), "namespaces/docs/");
    }

    #[test]
    fn key_shapes() {
        let l = Layout::default();
        assert_eq!(l.current_pointer_key("docs"), "namespaces/docs/CURRENT");
        assert_eq!(
            l.manifest_key("docs", 7),
            "namespaces/docs/manifests/00000000000000000007.json"
        );
        assert_eq!(
            l.wal_key("docs", 42),
            "namespaces/docs/wal/00000000000000000042.wal"
        );
        assert_eq!(
            l.segment_key("docs", "seg-abc123", "vectors.ivf"),
            "namespaces/docs/segments/seg-abc123/vectors.ivf"
        );
    }

    #[test]
    fn lexicographic_order_equals_numeric_order() {
        let l = Layout::default();
        let k9 = l.wal_key("docs", 9);
        let k10 = l.wal_key("docs", 10);
        let k_big = l.wal_key("docs", u64::MAX);
        assert!(k9 < k10, "{k9} should sort before {k10}");
        assert!(k10 < k_big);
        // u64::MAX fits in 20 digits without truncation.
        assert_eq!(Layout::wal_seq_from_key(&k_big), Some(u64::MAX));
    }

    #[test]
    fn key_parsing() {
        let l = Layout::new("t");
        let wal = l.wal_key("docs", 123);
        assert_eq!(Layout::wal_seq_from_key(&wal), Some(123));
        assert_eq!(Layout::wal_seq_from_key("namespaces/docs/CURRENT"), None);
        assert_eq!(
            Layout::wal_seq_from_key("namespaces/docs/segments/x/0001.wal"),
            None
        );

        let m = l.manifest_key("docs", 5);
        assert_eq!(Layout::manifest_generation_from_key(&m), Some(5));
        assert_eq!(Layout::manifest_generation_from_key(&wal), None);
    }

    #[test]
    fn ownership_rule() {
        let l = Layout::default();
        let parent_seg = l.segment_key("base", "seg-1", "vectors.ivf");
        // A branch referencing a parent segment does not own it.
        assert!(l.key_in_namespace("base", &parent_seg));
        assert!(!l.key_in_namespace("branch", &parent_seg));
        // Prefix matching is exact: "base2" must not own "base" keys.
        assert!(!l.key_in_namespace("bas", &parent_seg));
        assert!(!l.key_in_namespace("base2", &parent_seg));
    }
}
