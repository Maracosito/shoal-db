//! Typed attribute values.
//!
//! ReefDB attributes are typed scalars or homogeneous arrays. We keep the type
//! lattice deliberately small so attribute columns in segment files can use
//! compact, type-specific encodings, and so filter semantics stay simple and
//! predictable.
//!
//! The enum derives plain serde `Serialize`/`Deserialize` (externally tagged),
//! which works with non-self-describing formats such as bincode used by the
//! WAL. JSON interop for the HTTP API goes through the explicit
//! [`Value::from_json`] / [`Value::to_json`] conversions, which map values to
//! and from *natural* JSON (e.g. `42`, `"a"`, `["x","y"]`) rather than the
//! tagged serde form.

use crate::errors::ValidationError;
use serde::{Deserialize, Serialize};

/// A typed attribute value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    StringArray(Vec<String>),
    I64Array(Vec<i64>),
    F64Array(Vec<f64>),
}

impl Value {
    /// Human-readable type name, used in error messages and schema reporting.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) => "i64",
            Value::F64(_) => "f64",
            Value::String(_) => "string",
            Value::StringArray(_) => "string[]",
            Value::I64Array(_) => "i64[]",
            Value::F64Array(_) => "f64[]",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Convert a natural JSON value into a typed [`Value`].
    ///
    /// Rules:
    /// - JSON integers become `I64`; other numbers become `F64`.
    /// - Arrays must be homogeneous: all strings, all integers, or all
    ///   numbers (mixed int/float arrays widen to `F64Array`).
    /// - Empty arrays become an empty `StringArray` (the most common case;
    ///   filter semantics treat empty arrays uniformly).
    /// - JSON objects are rejected: nested objects are not supported in v1.
    pub fn from_json(v: &serde_json::Value) -> Result<Value, ValidationError> {
        match v {
            serde_json::Value::Null => Ok(Value::Null),
            serde_json::Value::Bool(b) => Ok(Value::Bool(*b)),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(Value::I64(i))
                } else if let Some(f) = n.as_f64() {
                    if !f.is_finite() {
                        return Err(ValidationError::InvalidValue(
                            "non-finite number".to_string(),
                        ));
                    }
                    Ok(Value::F64(f))
                } else {
                    Err(ValidationError::InvalidValue(format!(
                        "number out of range: {n}"
                    )))
                }
            }
            serde_json::Value::String(s) => Ok(Value::String(s.clone())),
            serde_json::Value::Array(items) => {
                if items.is_empty() {
                    return Ok(Value::StringArray(Vec::new()));
                }
                if items.iter().all(|x| x.is_string()) {
                    Ok(Value::StringArray(
                        items
                            .iter()
                            .map(|x| x.as_str().unwrap().to_string())
                            .collect(),
                    ))
                } else if items.iter().all(|x| x.is_i64()) {
                    Ok(Value::I64Array(
                        items.iter().map(|x| x.as_i64().unwrap()).collect(),
                    ))
                } else if items.iter().all(|x| x.is_number()) {
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        let f = x.as_f64().ok_or_else(|| {
                            ValidationError::InvalidValue(format!("number out of range: {x}"))
                        })?;
                        if !f.is_finite() {
                            return Err(ValidationError::InvalidValue(
                                "non-finite number in array".to_string(),
                            ));
                        }
                        out.push(f);
                    }
                    Ok(Value::F64Array(out))
                } else {
                    Err(ValidationError::UnsupportedJson(
                        "arrays must be homogeneous (all strings or all numbers)".to_string(),
                    ))
                }
            }
            serde_json::Value::Object(_) => Err(ValidationError::UnsupportedJson(
                "nested objects are not supported as attribute values in v1".to_string(),
            )),
        }
    }

    /// Convert back to natural JSON for API responses and exports.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::I64(i) => serde_json::Value::Number((*i).into()),
            Value::F64(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::StringArray(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
            Value::I64Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|i| serde_json::Value::Number((*i).into()))
                    .collect(),
            ),
            Value::F64Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|f| {
                        serde_json::Number::from_f64(*f)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    })
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_json_scalars() {
        assert_eq!(Value::from_json(&json!(null)).unwrap(), Value::Null);
        assert_eq!(Value::from_json(&json!(true)).unwrap(), Value::Bool(true));
        assert_eq!(Value::from_json(&json!(42)).unwrap(), Value::I64(42));
        assert_eq!(Value::from_json(&json!(-7)).unwrap(), Value::I64(-7));
        assert_eq!(Value::from_json(&json!(1.5)).unwrap(), Value::F64(1.5));
        assert_eq!(
            Value::from_json(&json!("hello")).unwrap(),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn from_json_arrays() {
        assert_eq!(
            Value::from_json(&json!(["a", "b"])).unwrap(),
            Value::StringArray(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            Value::from_json(&json!([1, 2, 3])).unwrap(),
            Value::I64Array(vec![1, 2, 3])
        );
        assert_eq!(
            Value::from_json(&json!([1, 2.5])).unwrap(),
            Value::F64Array(vec![1.0, 2.5])
        );
        assert_eq!(
            Value::from_json(&json!([])).unwrap(),
            Value::StringArray(vec![])
        );
    }

    #[test]
    fn from_json_rejects_mixed_and_objects() {
        assert!(Value::from_json(&json!([1, "a"])).is_err());
        assert!(Value::from_json(&json!({"a": 1})).is_err());
        assert!(Value::from_json(&json!([{"a": 1}])).is_err());
    }

    #[test]
    fn json_roundtrip() {
        let cases = vec![
            json!(null),
            json!(true),
            json!(123),
            json!(1.25),
            json!("text"),
            json!(["x", "y"]),
            json!([1, 2]),
            json!([0.5, 1.5]),
        ];
        for j in cases {
            let v = Value::from_json(&j).unwrap();
            assert_eq!(v.to_json(), j, "roundtrip failed for {j}");
        }
    }

    #[test]
    fn bincode_roundtrip() {
        let vals = vec![
            Value::Null,
            Value::Bool(false),
            Value::I64(-99),
            Value::F64(3.75),
            Value::String("s".to_string()),
            Value::StringArray(vec!["a".to_string()]),
            Value::I64Array(vec![1, 2]),
            Value::F64Array(vec![0.1]),
        ];
        for v in vals {
            let buf = bincode::serialize(&v).unwrap();
            let back: Value = bincode::deserialize(&buf).unwrap();
            assert_eq!(v, back);
        }
    }
}
