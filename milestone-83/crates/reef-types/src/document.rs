//! Documents, document IDs, and sparse vectors.

use crate::errors::ValidationError;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// Maximum length of a document ID in bytes.
pub const MAX_DOC_ID_BYTES: usize = 512;

/// Maximum length of an attribute name in characters.
pub const MAX_ATTR_NAME_CHARS: usize = 255;

/// A validated document identifier. IDs are arbitrary UTF-8 strings chosen by
/// the user (UUIDs, paths, hashes, integers-as-strings). They must be
/// non-empty, at most [`MAX_DOC_ID_BYTES`] bytes, and contain no control
/// characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocId(String);

impl DocId {
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ValidationError::InvalidId("id must not be empty".into()));
        }
        if s.len() > MAX_DOC_ID_BYTES {
            return Err(ValidationError::InvalidId(format!(
                "id is {} bytes; max is {}",
                s.len(),
                MAX_DOC_ID_BYTES
            )));
        }
        if s.chars().any(|c| c.is_control()) {
            return Err(ValidationError::InvalidId(
                "id must not contain control characters".into(),
            ));
        }
        Ok(DocId(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DocId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Validate an attribute name. Names are 1..=255 chars of `[A-Za-z0-9_.-]`,
/// and must not start with `__` (reserved for internal columns such as the
/// tombstone marker).
pub fn validate_attr_name(name: &str) -> Result<(), ValidationError> {
    if name.is_empty() {
        return Err(ValidationError::InvalidAttrName {
            name: name.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    if name.chars().count() > MAX_ATTR_NAME_CHARS {
        return Err(ValidationError::InvalidAttrName {
            name: name.to_string(),
            reason: format!("longer than {MAX_ATTR_NAME_CHARS} characters"),
        });
    }
    if name.starts_with("__") {
        return Err(ValidationError::InvalidAttrName {
            name: name.to_string(),
            reason: "names starting with '__' are reserved".to_string(),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(ValidationError::InvalidAttrName {
            name: name.to_string(),
            reason: "allowed characters are [A-Za-z0-9_.-]".to_string(),
        });
    }
    Ok(())
}

/// A sparse vector in coordinate form: strictly increasing dimension indices
/// paired with finite, non-zero weights. Used for SPLADE-style learned sparse
/// retrieval and classic term-weight vectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    pub fn new(indices: Vec<u32>, values: Vec<f32>) -> Result<Self, ValidationError> {
        let sv = SparseVector { indices, values };
        sv.validate()?;
        Ok(sv)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.indices.len() != self.values.len() {
            return Err(ValidationError::InvalidSparse(format!(
                "indices ({}) and values ({}) length mismatch",
                self.indices.len(),
                self.values.len()
            )));
        }
        if self.indices.is_empty() {
            return Err(ValidationError::InvalidSparse(
                "sparse vector must not be empty (omit it instead)".to_string(),
            ));
        }
        for w in self.indices.windows(2) {
            if w[0] >= w[1] {
                return Err(ValidationError::InvalidSparse(
                    "indices must be strictly increasing".to_string(),
                ));
            }
        }
        if self.values.iter().any(|v| !v.is_finite()) {
            return Err(ValidationError::InvalidSparse(
                "values must be finite".to_string(),
            ));
        }
        Ok(())
    }

    pub fn nnz(&self) -> usize {
        self.indices.len()
    }
}

/// A document: an ID, an optional dense vector, an optional sparse vector,
/// and a map of typed attributes. Text fields intended for full-text search
/// are ordinary `String` attributes; which fields are FTS-indexed is
/// namespace configuration, not part of the document itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse: Option<SparseVector>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

impl Document {
    pub fn new(id: DocId) -> Self {
        Document {
            id,
            vector: None,
            sparse: None,
            attributes: BTreeMap::new(),
        }
    }

    pub fn with_vector(mut self, v: Vec<f32>) -> Self {
        self.vector = Some(v);
        self
    }

    pub fn with_sparse(mut self, s: SparseVector) -> Self {
        self.sparse = Some(s);
        self
    }

    pub fn with_attr(mut self, name: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(name.into(), value);
        self
    }

    /// Validate the document's own invariants. Vector dimensionality
    /// consistency against the namespace is enforced by the engine, which
    /// knows the namespace configuration.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(v) = &self.vector {
            if v.is_empty() {
                return Err(ValidationError::InvalidVector(
                    "vector must not be empty (omit it instead)".to_string(),
                ));
            }
            if v.iter().any(|x| !x.is_finite()) {
                return Err(ValidationError::InvalidVector(
                    "vector components must be finite".to_string(),
                ));
            }
        }
        if let Some(s) = &self.sparse {
            s.validate()?;
        }
        for name in self.attributes.keys() {
            validate_attr_name(name)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_id_validation() {
        assert!(DocId::new("doc-1").is_ok());
        assert!(DocId::new("ユニコード").is_ok());
        assert!(DocId::new("").is_err());
        assert!(DocId::new("a\nb").is_err());
        assert!(DocId::new("x".repeat(MAX_DOC_ID_BYTES + 1)).is_err());
        assert!(DocId::new("x".repeat(MAX_DOC_ID_BYTES)).is_ok());
    }

    #[test]
    fn attr_name_validation() {
        assert!(validate_attr_name("title").is_ok());
        assert!(validate_attr_name("user.id-2_x").is_ok());
        assert!(validate_attr_name("").is_err());
        assert!(validate_attr_name("__deleted").is_err());
        assert!(validate_attr_name("has space").is_err());
        assert!(validate_attr_name(&"a".repeat(256)).is_err());
    }

    #[test]
    fn sparse_vector_validation() {
        assert!(SparseVector::new(vec![1, 5, 9], vec![0.5, 0.2, 0.1]).is_ok());
        assert!(SparseVector::new(vec![1, 1], vec![0.5, 0.2]).is_err());
        assert!(SparseVector::new(vec![5, 1], vec![0.5, 0.2]).is_err());
        assert!(SparseVector::new(vec![1], vec![0.5, 0.2]).is_err());
        assert!(SparseVector::new(vec![], vec![]).is_err());
        assert!(SparseVector::new(vec![1], vec![f32::NAN]).is_err());
    }

    #[test]
    fn document_validation() {
        let ok = Document::new(DocId::new("a").unwrap())
            .with_vector(vec![0.1, 0.2])
            .with_attr("title", Value::String("hello".to_string()));
        assert!(ok.validate().is_ok());

        let bad_vec = Document::new(DocId::new("a").unwrap()).with_vector(vec![]);
        assert!(bad_vec.validate().is_err());

        let nan_vec = Document::new(DocId::new("a").unwrap()).with_vector(vec![f32::NAN]);
        assert!(nan_vec.validate().is_err());

        let bad_attr =
            Document::new(DocId::new("a").unwrap()).with_attr("__x", Value::Bool(true));
        assert!(bad_attr.validate().is_err());
    }

    #[test]
    fn document_serde_roundtrip() {
        let doc = Document::new(DocId::new("d1").unwrap())
            .with_vector(vec![1.0, 2.0, 3.0])
            .with_sparse(SparseVector::new(vec![3, 7], vec![0.4, 0.6]).unwrap())
            .with_attr("rank", Value::I64(5))
            .with_attr("tags", Value::StringArray(vec!["x".to_string()]));
        let bin = bincode::serialize(&doc).unwrap();
        let back: Document = bincode::deserialize(&bin).unwrap();
        assert_eq!(doc, back);

        let js = serde_json::to_string(&doc).unwrap();
        let back2: Document = serde_json::from_str(&js).unwrap();
        assert_eq!(doc, back2);
    }
}
