//! Cross-version compatibility tests for the manifest format, exercising the
//! normative rules from `docs/storage-format.md`:
//!
//! * same major / older-or-equal minor → fully readable,
//! * same major / newer minor → readable, unknown fields preserved on rewrite,
//! * newer major → rejected.

use gannet_format::manifest::{ManifestError, ManifestPointer, NamespaceManifest};
use gannet_format::version::FormatVersion;

const FUTURE_MINOR_MANIFEST: &str = r#"{
    "format": "1.7",
    "namespace_id": "docs",
    "generation": 12,
    "created_at": "2025-01-01T00:00:00Z",
    "updated_at": "2025-06-01T12:30:00Z",
    "wal": [],
    "segments": [],
    "stats": {"approx_doc_count": 5},
    "compression_codec": "zstd-v2",
    "experimental_block": {"nested": [1, 2, 3]}
}"#;

#[test]
fn reads_future_minor_version_and_preserves_unknown_fields() {
    let m = NamespaceManifest::from_json(FUTURE_MINOR_MANIFEST.as_bytes())
        .expect("minor-version bump must remain readable");
    assert_eq!(m.format, FormatVersion::new(1, 7));
    assert_eq!(m.namespace_id, "docs");
    assert_eq!(m.generation, 12);
    assert_eq!(m.stats.approx_doc_count, 5);

    // Unknown fields landed in `extra`...
    assert!(m.extra.contains_key("compression_codec"));
    assert!(m.extra.contains_key("experimental_block"));

    // ...and survive a rewrite byte-for-byte in value terms.
    let rewritten = m.to_json().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
    assert_eq!(value["compression_codec"], "zstd-v2");
    assert_eq!(value["experimental_block"]["nested"][1], 2);
}

#[test]
fn rejects_future_major_version() {
    let raw = FUTURE_MINOR_MANIFEST.replace("\"format\": \"1.7\"", "\"format\": \"2.0\"");
    let err = NamespaceManifest::from_json(raw.as_bytes())
        .expect_err("major-version bump must be rejected");
    match err {
        ManifestError::IncompatibleVersion { found, .. } => {
            assert_eq!(found, FormatVersion::new(2, 0));
        }
        other => panic!("expected IncompatibleVersion, got {other:?}"),
    }
}

#[test]
fn rejects_garbage_bytes() {
    assert!(NamespaceManifest::from_json(b"\x00\x01not json").is_err());
    assert!(NamespaceManifest::from_json(b"{}").is_err(), "missing required fields");
}

#[test]
fn pointer_compat_rules_match_manifest_rules() {
    let ok = r#"{"format": "1.3", "generation": 4,
                 "manifest_key": "namespaces/docs/manifests/00000000000000000004.json",
                 "fence_token": "abc"}"#;
    let p = ManifestPointer::from_json(ok.as_bytes()).unwrap();
    assert_eq!(p.generation, 4);
    assert!(p.extra.contains_key("fence_token"));

    let bad = ok.replace("1.3", "3.0");
    assert!(matches!(
        ManifestPointer::from_json(bad.as_bytes()),
        Err(ManifestError::IncompatibleVersion { .. })
    ));
}

#[test]
fn stable_round_trip_through_serde_value() {
    // A manifest must survive JSON -> struct -> JSON -> struct unchanged.
    let m = NamespaceManifest::from_json(FUTURE_MINOR_MANIFEST.as_bytes()).unwrap();
    let bytes = m.to_json().unwrap();
    let again = NamespaceManifest::from_json(&bytes).unwrap();
    assert_eq!(m, again);
}
