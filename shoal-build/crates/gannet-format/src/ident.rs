//! Validation rules for identifiers that end up inside object-storage keys.
//!
//! Namespace IDs are embedded directly in object keys, so the allowed
//! alphabet is deliberately strict: lowercase ASCII letters, digits, `-`,
//! and `_`, starting with a letter or digit. This rules out path traversal
//! (`..`), URL-encoding ambiguity, and case-sensitivity surprises across
//! S3-compatible stores.
//!
//! Document IDs never appear in object keys (documents live inside WAL
//! records and segments), so they are far more permissive: any non-empty
//! UTF-8 string up to 512 bytes without control characters.

/// Maximum byte length of a namespace ID.
pub const MAX_NAMESPACE_ID_LEN: usize = 128;

/// Maximum byte length of a document ID.
pub const MAX_DOCUMENT_ID_LEN: usize = 512;

/// Identifier validation failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentError {
    #[error("identifier is empty")]
    Empty,
    #[error("identifier is too long ({len} bytes > {max} max)")]
    TooLong { len: usize, max: usize },
    #[error("invalid character {ch:?} in identifier")]
    InvalidChar { ch: char },
    #[error("identifier must start with a lowercase ASCII letter or digit")]
    InvalidStart,
}

/// Validate a namespace ID against the storage-safe alphabet.
pub fn validate_namespace_id(id: &str) -> Result<(), IdentError> {
    if id.is_empty() {
        return Err(IdentError::Empty);
    }
    if id.len() > MAX_NAMESPACE_ID_LEN {
        return Err(IdentError::TooLong {
            len: id.len(),
            max: MAX_NAMESPACE_ID_LEN,
        });
    }
    let first = id.chars().next().expect("non-empty");
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(IdentError::InvalidStart);
    }
    for ch in id.chars() {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_';
        if !ok {
            return Err(IdentError::InvalidChar { ch });
        }
    }
    Ok(())
}

/// Validate a document ID (permissive: UTF-8, bounded, no control chars).
pub fn validate_document_id(id: &str) -> Result<(), IdentError> {
    if id.is_empty() {
        return Err(IdentError::Empty);
    }
    if id.len() > MAX_DOCUMENT_ID_LEN {
        return Err(IdentError::TooLong {
            len: id.len(),
            max: MAX_DOCUMENT_ID_LEN,
        });
    }
    if let Some(ch) = id.chars().find(|c| c.is_control()) {
        return Err(IdentError::InvalidChar { ch });
    }
    Ok(())
}

/// Validate an attribute (metadata field) name.
pub fn validate_attribute_name(name: &str) -> Result<(), IdentError> {
    if name.is_empty() {
        return Err(IdentError::Empty);
    }
    if name.len() > 256 {
        return Err(IdentError::TooLong {
            len: name.len(),
            max: 256,
        });
    }
    if let Some(ch) = name.chars().find(|c| c.is_control()) {
        return Err(IdentError::InvalidChar { ch });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_namespace_ids() {
        for ok in ["docs", "my-app_prod", "a", "ns01", "0start", "x".repeat(128).as_str()] {
            assert_eq!(validate_namespace_id(ok), Ok(()), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_invalid_namespace_ids() {
        assert_eq!(validate_namespace_id(""), Err(IdentError::Empty));
        assert!(matches!(
            validate_namespace_id("-leading-dash"),
            Err(IdentError::InvalidStart)
        ));
        assert!(matches!(
            validate_namespace_id("_leading_underscore"),
            Err(IdentError::InvalidStart)
        ));
        assert!(matches!(
            validate_namespace_id("Has-Upper"),
            Err(IdentError::InvalidStart)
        ));
        assert!(matches!(
            validate_namespace_id("dot.segment"),
            Err(IdentError::InvalidChar { ch: '.' })
        ));
        assert!(matches!(
            validate_namespace_id("slash/attack"),
            Err(IdentError::InvalidChar { ch: '/' })
        ));
        let long = "a".repeat(129);
        assert!(matches!(
            validate_namespace_id(&long),
            Err(IdentError::TooLong { .. })
        ));
    }

    #[test]
    fn document_ids_are_permissive_but_bounded() {
        assert_eq!(validate_document_id("doc-1"), Ok(()));
        assert_eq!(validate_document_id("user:42/profile §"), Ok(()));
        assert_eq!(validate_document_id("日本語のID"), Ok(()));
        assert_eq!(validate_document_id(""), Err(IdentError::Empty));
        assert!(matches!(
            validate_document_id("has\nnewline"),
            Err(IdentError::InvalidChar { .. })
        ));
        let long = "a".repeat(513);
        assert!(matches!(
            validate_document_id(&long),
            Err(IdentError::TooLong { .. })
        ));
    }

    #[test]
    fn attribute_names() {
        assert_eq!(validate_attribute_name("genre"), Ok(()));
        assert_eq!(validate_attribute_name("user.tier"), Ok(()));
        assert_eq!(validate_attribute_name(""), Err(IdentError::Empty));
        assert!(matches!(
            validate_attribute_name("bad\tname"),
            Err(IdentError::InvalidChar { .. })
        ));
    }
}
