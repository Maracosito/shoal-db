//! On-storage key layout.
//!
//! ```text
//! registry/registry.json                       org/project/namespace registry
//! registry/segrefs.json                        segment reference counts (COW branching)
//! namespaces/{ns_id}/manifest.json             namespace manifest
//! namespaces/{ns_id}/wal/{seq:020}.wal         one WAL object per write batch
//! namespaces/{ns_id}/segments/{seg_id}.seg     immutable segment
//! ```
//!
//! Branches reference segments by **full key**, so a branch manifest may point
//! at segments physically located under its parent's prefix. Physical deletion
//! only ever happens when the global refcount reaches zero.

pub const REGISTRY_KEY: &str = "registry/registry.json";
pub const SEGREFS_KEY: &str = "registry/segrefs.json";

/// Root prefix for a project within an org: `orgs/{org}/projects/{project}/`
pub fn project_root(org: &str, project: &str) -> String {
    format!("orgs/{}/projects/{}/", org, project)
}

/// Root prefix for a namespace: `orgs/{org}/projects/{project}/namespaces/{ns}/`
pub fn namespace_root(org: &str, project: &str, ns: &str) -> String {
    format!("orgs/{}/projects/{}/namespaces/{}/", org, project, ns)
}

pub fn namespace_prefix(ns_id: &str) -> String {
    format!("namespaces/{}/", ns_id)
}

pub fn manifest_key(ns_id: &str) -> String {
    format!("namespaces/{}/manifest.json", ns_id)
}

pub fn wal_prefix(ns_id: &str) -> String {
    format!("namespaces/{}/wal/", ns_id)
}

pub fn wal_key(ns_id: &str, seq: u64) -> String {
    format!("namespaces/{}/wal/{:020}.wal", ns_id, seq)
}

/// Extract the WAL sequence number from a full WAL object key.
pub fn parse_wal_seq(key: &str) -> Option<u64> {
    let name = key.rsplit('/').next()?;
    let stem = name.strip_suffix(".wal")?;
    stem.parse::<u64>().ok()
}

pub fn segments_prefix(ns_id: &str) -> String {
    format!("namespaces/{}/segments/", ns_id)
}

pub fn segment_key(ns_id: &str, segment_id: &str) -> String {
    format!("namespaces/{}/segments/{}.seg", ns_id, segment_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_keys_sort_lexically() {
        let a = wal_key("ns", 9);
        let b = wal_key("ns", 10);
        assert!(a < b);
        assert_eq!(parse_wal_seq(&a), Some(9));
        assert_eq!(parse_wal_seq("namespaces/x/wal/garbage"), None);
    }
}
