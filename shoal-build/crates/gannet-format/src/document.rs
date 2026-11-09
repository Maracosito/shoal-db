//! The document model shared by the API layer, WAL records, and segments.
//!
//! A document has an ID, an optional dense vector, an optional sparse
//! vector, and a flat map of metadata attributes. This mirrors the
//! `Document` schema in `api/openapi.yaml`.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ident::{validate_attribute_name, validate_document_id, IdentError};

/// A document identifier (an opaque, bounded UTF-8 string).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(pub String);

impl DocumentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DocumentId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// A metadata attribute value.
///
/// JSON-shaped, but with integers and floats kept distinct so attribute
/// indexes can store integers exactly. Equality and ordering between `Int`
/// and `Float` coerce through `f64` (so the filter `year >= 1990` matches a
/// document where `year` was ingested as `1990.0`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<AttributeValue>),
}

impl AttributeValue {
    fn as_f64(&self) -> Option<f64> {
        match self {
            AttributeValue::Int(i) => Some(*i as f64),
            AttributeValue::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Total-ish comparison used by range filters. Returns `None` for
    /// incomparable type pairs (e.g. string vs number) — range predicates
    /// over incomparable values evaluate to `false`.
    pub fn compare(&self, other: &AttributeValue) -> Option<Ordering> {
        use AttributeValue::*;
        match (self, other) {
            (Null, Null) => Some(Ordering::Equal),
            (Bool(a), Bool(b)) => Some(a.cmp(b)),
            (String(a), String(b)) => Some(a.cmp(b)),
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b),
                _ => None,
            },
        }
    }

    /// Equality with numeric coercion (`Int(1)` equals `Float(1.0)`).
    /// Arrays compare element-wise.
    pub fn loosely_equals(&self, other: &AttributeValue) -> bool {
        use AttributeValue::*;
        match (self, other) {
            (Array(a), Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.loosely_equals(y))
            }
            _ => self.compare(other) == Some(Ordering::Equal),
        }
    }
}

/// A sparse vector: parallel arrays of strictly increasing dimension
/// indices and their weights.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

/// Sparse vector validation failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SparseVectorError {
    #[error("indices and values have different lengths ({indices} vs {values})")]
    LengthMismatch { indices: usize, values: usize },
    #[error("indices must be strictly increasing (violation at position {position})")]
    NotStrictlyIncreasing { position: usize },
    #[error("sparse vector value at position {position} is not finite")]
    NonFiniteValue { position: usize },
}

impl SparseVector {
    pub fn validate(&self) -> Result<(), SparseVectorError> {
        if self.indices.len() != self.values.len() {
            return Err(SparseVectorError::LengthMismatch {
                indices: self.indices.len(),
                values: self.values.len(),
            });
        }
        for (i, w) in self.indices.windows(2).enumerate() {
            if w[1] <= w[0] {
                return Err(SparseVectorError::NotStrictlyIncreasing { position: i + 1 });
            }
        }
        for (i, v) in self.values.iter().enumerate() {
            if !v.is_finite() {
                return Err(SparseVectorError::NonFiniteValue { position: i });
            }
        }
        Ok(())
    }
}

/// Document validation failure.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DocumentError {
    #[error("invalid document id: {0}")]
    InvalidId(IdentError),
    #[error("dense vector is empty")]
    EmptyVector,
    #[error("dense vector component at index {index} is not finite")]
    NonFiniteVector { index: usize },
    #[error("invalid sparse vector: {0}")]
    Sparse(#[from] SparseVectorError),
    #[error("invalid attribute name {name:?}: {source}")]
    InvalidAttributeName { name: String, source: IdentError },
}

/// A full document as accepted by the upsert API and stored in WAL records.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: DocumentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, AttributeValue>,
}

impl Document {
    pub fn validate(&self) -> Result<(), DocumentError> {
        validate_document_id(self.id.as_str()).map_err(DocumentError::InvalidId)?;
        if let Some(v) = &self.vector {
            if v.is_empty() {
                return Err(DocumentError::EmptyVector);
            }
            if let Some(index) = v.iter().position(|x| !x.is_finite()) {
                return Err(DocumentError::NonFiniteVector { index });
            }
        }
        if let Some(sv) = &self.sparse_vector {
            sv.validate()?;
        }
        for name in self.attributes.keys() {
            validate_attribute_name(name).map_err(|source| DocumentError::InvalidAttributeName {
                name: name.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(json: &str) -> Document {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn attribute_value_untagged_serde() {
        let v: AttributeValue = serde_json::from_str("42").unwrap();
        assert_eq!(v, AttributeValue::Int(42));
        let v: AttributeValue = serde_json::from_str("42.5").unwrap();
        assert_eq!(v, AttributeValue::Float(42.5));
        let v: AttributeValue = serde_json::from_str("\"hi\"").unwrap();
        assert_eq!(v, AttributeValue::String("hi".into()));
        let v: AttributeValue = serde_json::from_str("null").unwrap();
        assert_eq!(v, AttributeValue::Null);
        let v: AttributeValue = serde_json::from_str("[1, \"a\"]").unwrap();
        assert_eq!(
            v,
            AttributeValue::Array(vec![
                AttributeValue::Int(1),
                AttributeValue::String("a".into())
            ])
        );
    }

    #[test]
    fn numeric_coercion() {
        assert!(AttributeValue::Int(1).loosely_equals(&AttributeValue::Float(1.0)));
        assert_eq!(
            AttributeValue::Int(2).compare(&AttributeValue::Float(1.5)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            AttributeValue::String("a".into()).compare(&AttributeValue::Int(1)),
            None
        );
    }

    #[test]
    fn document_serde_round_trip() {
        let d = doc(
            r#"{
                "id": "doc-1",
                "vector": [0.1, 0.2, 0.3],
                "sparse_vector": {"indices": [3, 17, 99], "values": [0.5, 1.0, 0.25]},
                "attributes": {"genre": "sci-fi", "year": 1979, "tags": ["a", "b"]}
            }"#,
        );
        d.validate().unwrap();
        let json = serde_json::to_string(&d).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn minimal_document_is_valid() {
        let d = doc(r#"{"id": "x"}"#);
        d.validate().unwrap();
        assert!(d.vector.is_none());
        assert!(d.attributes.is_empty());
    }

    #[test]
    fn validation_catches_bad_vectors() {
        let d = Document {
            id: DocumentId::from("a"),
            vector: Some(vec![]),
            sparse_vector: None,
            attributes: BTreeMap::new(),
        };
        assert_eq!(d.validate(), Err(DocumentError::EmptyVector));

        let d = Document {
            id: DocumentId::from("a"),
            vector: Some(vec![1.0, f32::NAN]),
            sparse_vector: None,
            attributes: BTreeMap::new(),
        };
        assert_eq!(d.validate(), Err(DocumentError::NonFiniteVector { index: 1 }));
    }

    #[test]
    fn validation_catches_bad_sparse_vectors() {
        let sv = SparseVector {
            indices: vec![1, 1],
            values: vec![0.5, 0.5],
        };
        assert_eq!(
            sv.validate(),
            Err(SparseVectorError::NotStrictlyIncreasing { position: 1 })
        );

        let sv = SparseVector {
            indices: vec![1, 2, 3],
            values: vec![0.5, 0.5],
        };
        assert!(matches!(
            sv.validate(),
            Err(SparseVectorError::LengthMismatch { .. })
        ));
    }
}
