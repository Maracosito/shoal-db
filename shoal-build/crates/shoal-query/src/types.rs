//! Core document model shared by the vector, full-text and filter engines.
//!
//! The query engine identifies documents by a **document ordinal**
//! ([`DocOrd`]): a dense `u32` assigned by the storage layer within a
//! namespace snapshot. Ordinals are what indexes, bitmaps and posting lists
//! store; the executor maps them back to external string IDs when building
//! responses. Keeping ordinals at 32 bits lets us use Roaring bitmaps for
//! filter results and keeps on-disk postings compact.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{QueryError, Result};

/// Dense per-snapshot document ordinal.
pub type DocOrd = u32;

/// An attribute value attached to a document.
///
/// JSON maps naturally onto this enum (`null`, booleans, integers, floats,
/// strings, arrays). Arrays are only used with set-style operators
/// (`In` / `ContainsAny`); they have no defined ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Array(Vec<Value>),
}

impl Value {
    /// Human-readable type name, used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) => "integer",
            Value::F64(_) => "float",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
        }
    }

    /// Numeric view of the value, coercing integers to floats.
    /// Integers and floats are mutually comparable through this.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::I64(v) => Some(*v as f64),
            Value::F64(v) => Some(*v),
            _ => None,
        }
    }

    /// Returns `true` if this value is `Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Total-order comparison between two values *of compatible types*.
    ///
    /// Rules:
    /// - integers and floats compare numerically with each other,
    /// - strings compare lexicographically (byte order),
    /// - booleans compare with `false < true`,
    /// - everything else (cross-type, nulls, arrays) is incomparable and
    ///   returns `None`, which makes range filters on them match nothing.
    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::I64(a), Value::I64(b)) => Some(a.cmp(b)),
            _ => match (self.as_f64(), other.as_f64()) {
                (Some(a), Some(b)) => a.partial_cmp(&b),
                _ => None,
            },
        }
    }

    /// Equality used by filters: numeric values compare numerically across
    /// the integer/float divide (`1 == 1.0`); all other comparisons require
    /// matching types.
    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self.as_f64(), other.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => self == other,
        }
    }
}

/// A sparse vector: parallel arrays of strictly increasing term/feature
/// indices and their weights. Suitable for SPLADE-style learned sparse
/// representations or classic tf-idf vectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

impl SparseVector {
    /// Build a validated sparse vector. Indices must be strictly
    /// increasing and the two arrays must have equal length.
    pub fn new(indices: Vec<u32>, values: Vec<f32>) -> Result<Self> {
        if indices.len() != values.len() {
            return Err(QueryError::InvalidArgument(format!(
                "sparse vector has {} indices but {} values",
                indices.len(),
                values.len()
            )));
        }
        for w in indices.windows(2) {
            if w[0] >= w[1] {
                return Err(QueryError::InvalidArgument(
                    "sparse vector indices must be strictly increasing".to_string(),
                ));
            }
        }
        Ok(SparseVector { indices, values })
    }

    /// Number of non-zero entries.
    pub fn nnz(&self) -> usize {
        self.indices.len()
    }

    /// Dot product of two sorted sparse vectors (linear merge walk).
    pub fn dot(&self, other: &SparseVector) -> f32 {
        let mut i = 0usize;
        let mut j = 0usize;
        let mut sum = 0f32;
        while i < self.indices.len() && j < other.indices.len() {
            match self.indices[i].cmp(&other.indices[j]) {
                Ordering::Less => i += 1,
                Ordering::Greater => j += 1,
                Ordering::Equal => {
                    sum += self.values[i] * other.values[j];
                    i += 1;
                    j += 1;
                }
            }
        }
        sum
    }

    /// Euclidean norm of the sparse vector.
    pub fn l2_norm(&self) -> f32 {
        self.values.iter().map(|v| v * v).sum::<f32>().sqrt()
    }
}

/// A document as the query engine sees it: external ID, optional dense and
/// sparse vectors, named text fields, and arbitrary metadata attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub text: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

impl Document {
    /// Create an empty document with just an ID.
    pub fn new(id: impl Into<String>) -> Self {
        Document {
            id: id.into(),
            vector: None,
            sparse_vector: None,
            text: BTreeMap::new(),
            attributes: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_numeric_cross_type_compare() {
        assert_eq!(
            Value::I64(2).compare(&Value::F64(2.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Value::F64(1.5).compare(&Value::I64(2)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::I64(3).compare(&Value::I64(2)),
            Some(Ordering::Greater)
        );
        assert!(Value::I64(2).loose_eq(&Value::F64(2.0)));
        assert!(!Value::I64(2).loose_eq(&Value::F64(2.5)));
    }

    #[test]
    fn value_incomparable_types() {
        assert_eq!(Value::Str("a".into()).compare(&Value::I64(1)), None);
        assert_eq!(Value::Null.compare(&Value::Null), None);
        assert_eq!(
            Value::Array(vec![Value::I64(1)]).compare(&Value::Array(vec![Value::I64(1)])),
            None
        );
        assert_eq!(
            Value::Str("a".into()).compare(&Value::Str("b".into())),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Bool(false).compare(&Value::Bool(true)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn value_json_round_trip() {
        let v: Value = serde_json::from_str("3").unwrap();
        assert_eq!(v, Value::I64(3));
        let v: Value = serde_json::from_str("3.5").unwrap();
        assert_eq!(v, Value::F64(3.5));
        let v: Value = serde_json::from_str("null").unwrap();
        assert_eq!(v, Value::Null);
        let v: Value = serde_json::from_str(r#"["a", 1]"#).unwrap();
        assert_eq!(v, Value::Array(vec![Value::Str("a".into()), Value::I64(1)]));
    }

    #[test]
    fn sparse_vector_validation() {
        assert!(SparseVector::new(vec![1, 2, 3], vec![1.0, 2.0]).is_err());
        assert!(SparseVector::new(vec![3, 2], vec![1.0, 2.0]).is_err());
        assert!(SparseVector::new(vec![2, 2], vec![1.0, 2.0]).is_err());
        assert!(SparseVector::new(vec![1, 5, 9], vec![1.0, 2.0, 3.0]).is_ok());
    }

    #[test]
    fn sparse_dot_product() {
        let a = SparseVector::new(vec![1, 4, 7], vec![1.0, 2.0, 3.0]).unwrap();
        let b = SparseVector::new(vec![2, 4, 7, 9], vec![5.0, 0.5, 2.0, 4.0]).unwrap();
        // overlap at 4 (2.0*0.5=1.0) and 7 (3.0*2.0=6.0)
        assert!((a.dot(&b) - 7.0).abs() < 1e-6);
        assert!((b.dot(&a) - 7.0).abs() < 1e-6);
        let empty = SparseVector::new(vec![], vec![]).unwrap();
        assert_eq!(a.dot(&empty), 0.0);
    }
}
