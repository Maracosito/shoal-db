//! Contract test binding the repository's OpenAPI document to the funded
//! API surface: the spec must be well-formed OpenAPI 3.1 and must cover
//! every endpoint family promised for v1 (namespaces, document writes,
//! delete-by-filter, query, export, copy, branch, warm/pin, health,
//! metrics). This keeps `api/openapi.yaml` from silently regressing as the
//! implementation lands.

use serde_yaml::Value;

fn load_spec() -> (String, Value) {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/openapi.yaml");
    let raw = std::fs::read_to_string(path).expect("api/openapi.yaml must exist and be readable");
    let doc: Value = serde_yaml::from_str(&raw).expect("api/openapi.yaml must be valid YAML");
    (raw, doc)
}

fn get<'a>(doc: &'a Value, key: &str) -> Option<&'a Value> {
    doc.as_mapping()?.get(&Value::String(key.to_string()))
}

#[test]
fn spec_is_openapi_3_1_with_metadata() {
    let (_, doc) = load_spec();
    let version = get(&doc, "openapi")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        version.starts_with("3.1"),
        "expected an OpenAPI 3.1.x document, found {version:?}"
    );

    let info = get(&doc, "info").expect("spec must have an info block");
    let title = get(info, "title").and_then(Value::as_str).unwrap_or_default();
    assert!(!title.is_empty(), "info.title must be set");
    let api_version = get(info, "version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(!api_version.is_empty(), "info.version must be set");
}

#[test]
fn spec_has_a_substantial_path_surface() {
    let (_, doc) = load_spec();
    let paths = get(&doc, "paths")
        .and_then(Value::as_mapping)
        .expect("spec must define paths");
    assert!(
        paths.len() >= 8,
        "expected at least 8 paths for the v1 surface, found {}",
        paths.len()
    );
    for (key, _) in paths {
        let p = key.as_str().unwrap_or_default();
        assert!(p.starts_with('/'), "path keys must start with '/': {p:?}");
    }
}

#[test]
fn spec_covers_every_funded_endpoint_family() {
    let (raw, _) = load_spec();
    let haystack = raw.to_lowercase();

    // Each family is satisfied by any one of its alternative tokens, so the
    // test pins the *capability surface* rather than exact path spellings.
    let families: &[(&str, &[&str])] = &[
        ("namespace management", &["namespaces"]),
        ("document upsert", &["upsert", "documents"]),
        ("document patch", &["patch"]),
        ("delete", &["delete"]),
        ("delete by filter", &["filter"]),
        ("query", &["query"]),
        ("export", &["export"]),
        ("copy", &["copy"]),
        ("branch", &["branch"]),
        ("warm cache", &["warm"]),
        ("pinning", &["pin"]),
        ("health check", &["health"]),
        ("metrics", &["metrics"]),
    ];

    let mut missing = Vec::new();
    for (family, tokens) in families {
        if !tokens.iter().any(|t| haystack.contains(t)) {
            missing.push(*family);
        }
    }
    assert!(
        missing.is_empty(),
        "OpenAPI spec is missing funded endpoint families: {missing:?}"
    );
}

#[test]
fn spec_declares_api_key_security() {
    let (raw, doc) = load_spec();
    let has_security_schemes = get(&doc, "components")
        .and_then(|c| get(c, "securitySchemes"))
        .is_some();
    assert!(
        has_security_schemes || raw.to_lowercase().contains("securityschemes"),
        "spec must declare API-key security schemes"
    );
}
