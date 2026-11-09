//! Metadata filter engine.
//!
//! Filters constrain which documents a query may return. This module defines:
//!
//! * [`Value`] — the attribute value model (null, bool, int, float, string,
//!   array), with a JSON-natural serde representation;
//! * [`Filter`] — a serializable filter AST with comparison leaves
//!   (`Eq`, `NotEq`, `Gt`, `Gte`, `Lt`, `Lte`, `In`, `ContainsAny`,
//!   `StartsWith`, `Contains`) and boolean combinators (`And`, `Or`, `Not`);
//! * row-wise evaluation via [`Filter::matches`], used when scoring
//!   candidates fetched from segment data; and
//! * a persistent [`index::AttributeIndex`] that evaluates the same AST to a
//!   [`DocSet`] without touching per-document data — the fast path for
//!   filtered vector / full-text search.
//!
//! # Semantics
//!
//! The two evaluation strategies are defined to agree exactly (this is
//! enforced by randomized tests in `tests/filter_algebra.rs`):
//!
//! * **Missing / null fields**: every *leaf* predicate is `false` when the
//!   field is absent or `null`. In particular `NotEq` requires the field to be
//!   present — `NotEq(f, v)` is *not* the same as `Not(Eq(f, v))`, which also
//!   matches documents lacking `f`. This mirrors SQL-style tri-state logic
//!   collapsed to two values.
//! * **Arrays**: a leaf predicate on an array attribute matches if **any
//!   element** satisfies it (`tags = ["a","b"]` matches `Eq(tags, "a")`).
//!   Nested arrays are not indexed and never match. `In` and `ContainsAny`
//!   share these semantics; both are kept in the API because they express
//!   different intent (scalar membership vs. array overlap).
//! * **Numbers**: integers and floats compare numerically across types
//!   (`Eq(x, 1)` matches `1.0`). `NaN` never matches anything and is never
//!   indexed. `-0.0` and `0.0` are equal.
//! * **Ordering**: `Gt`/`Gte`/`Lt`/`Lte` are defined for numbers (numeric
//!   order) and strings (lexicographic byte order). Booleans and arrays of
//!   incomparable values do not satisfy range predicates.
//! * **Strings**: `StartsWith` is prefix match; `Contains` is substring
//!   match. Both are case-sensitive; callers wanting case-insensitive
//!   matching should normalize at ingest time.
//! * **Boolean combinators**: `And([])` is `true` (matches everything);
//!   `Or([])` is `false`; `Not` is complement over the segment's documents.

pub mod docset;
pub mod index;

pub use docset::DocSet;
pub use index::{AttributeIndex, AttributeIndexBuilder};

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// An attribute value attached to a document.
///
/// Serialized/deserialized untagged, so the JSON wire format is natural:
/// `null`, `true`, `42`, `4.2`, `"text"`, `[…]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    /// Explicit null. Treated identically to an absent field by filters.
    Null,
    /// Boolean.
    Bool(bool),
    /// 64-bit signed integer.
    Int(i64),
    /// 64-bit float.
    Float(f64),
    /// UTF-8 string.
    Str(String),
    /// Array of values. One level of nesting is meaningful to filters.
    Array(Vec<Value>),
}

impl Value {
    /// Returns the string if this is a `Str`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the boolean if this is a `Bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns a numeric view (ints widen to f64); `None` for non-numeric
    /// values and `NaN`.
    pub fn as_f64(&self) -> Option<f64> {
        as_num(self)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Value::Float(v as f64)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Str(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Str(v)
    }
}
impl From<Vec<Value>> for Value {
    fn from(v: Vec<Value>) -> Self {
        Value::Array(v)
    }
}

/// A document's attribute map.
pub type Attributes = BTreeMap<String, Value>;

/// Numeric view of a value: ints widen to `f64`; `NaN` is rejected so it can
/// never match or be indexed.
pub(crate) fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if !f.is_nan() => Some(*f),
        _ => None,
    }
}

/// The filter AST.
///
/// JSON wire format is internally tagged on `"op"`, e.g.
/// `{"op":"eq","field":"category","value":"news"}` or
/// `{"op":"and","filters":[…]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Filter {
    /// Field equals value (any array element for array attributes).
    Eq {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field is present and does not equal value.
    NotEq {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field strictly greater than value (numbers, strings).
    Gt {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field greater than or equal to value.
    Gte {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field strictly less than value.
    Lt {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field less than or equal to value.
    Lte {
        /// Attribute name.
        field: String,
        /// Comparison value.
        value: Value,
    },
    /// Field equals any of the listed values.
    In {
        /// Attribute name.
        field: String,
        /// Candidate values.
        values: Vec<Value>,
    },
    /// Array-overlap: any element of the field equals any listed value.
    /// (Identical evaluation to `In`; kept separate for API intent.)
    ContainsAny {
        /// Attribute name.
        field: String,
        /// Candidate values.
        values: Vec<Value>,
    },
    /// String field starts with the given prefix (case-sensitive).
    StartsWith {
        /// Attribute name.
        field: String,
        /// Prefix to match.
        value: String,
    },
    /// String field contains the given substring (case-sensitive).
    Contains {
        /// Attribute name.
        field: String,
        /// Substring to match.
        value: String,
    },
    /// All sub-filters match. `And([])` matches everything.
    And {
        /// Sub-filters.
        filters: Vec<Filter>,
    },
    /// Any sub-filter matches. `Or([])` matches nothing.
    Or {
        /// Sub-filters.
        filters: Vec<Filter>,
    },
    /// Complement of the inner filter over the segment's documents.
    Not {
        /// Inner filter.
        filter: Box<Filter>,
    },
}

impl Filter {
    /// `field == value`.
    pub fn eq(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::Eq {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field != value` (field must be present).
    pub fn not_eq(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::NotEq {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field > value`.
    pub fn gt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::Gt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field >= value`.
    pub fn gte(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::Gte {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field < value`.
    pub fn lt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::Lt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field <= value`.
    pub fn lte(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Filter::Lte {
            field: field.into(),
            value: value.into(),
        }
    }

    /// `field IN values`.
    pub fn is_in<I, V>(field: impl Into<String>, values: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<Value>,
    {
        Filter::In {
            field: field.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// Array-overlap predicate.
    pub fn contains_any<I, V>(field: impl Into<String>, values: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<Value>,
    {
        Filter::ContainsAny {
            field: field.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// String prefix predicate.
    pub fn starts_with(field: impl Into<String>, prefix: impl Into<String>) -> Self {
        Filter::StartsWith {
            field: field.into(),
            value: prefix.into(),
        }
    }

    /// Substring predicate.
    pub fn contains(field: impl Into<String>, needle: impl Into<String>) -> Self {
        Filter::Contains {
            field: field.into(),
            value: needle.into(),
        }
    }

    /// Conjunction.
    pub fn and(filters: Vec<Filter>) -> Self {
        Filter::And { filters }
    }

    /// Disjunction.
    pub fn or(filters: Vec<Filter>) -> Self {
        Filter::Or { filters }
    }

    /// Negation (consuming builder form of `Not`).
    pub fn negate(self) -> Self {
        Filter::Not {
            filter: Box::new(self),
        }
    }

    /// Row-wise evaluation of the filter against a single document's
    /// attributes. Guaranteed to agree with
    /// [`index::AttributeIndex::eval`] for documents present in the index.
    pub fn matches(&self, attrs: &Attributes) -> bool {
        match self {
            Filter::And { filters } => filters.iter().all(|f| f.matches(attrs)),
            Filter::Or { filters } => filters.iter().any(|f| f.matches(attrs)),
            Filter::Not { filter } => !filter.matches(attrs),
            Filter::Eq { field, value } => with_attr(attrs, field, |a| leaf_match_eq(a, value)),
            Filter::NotEq { field, value } => {
                with_attr(attrs, field, |a| !leaf_match_eq(a, value))
            }
            Filter::Gt { field, value } => with_attr(attrs, field, |a| {
                leaf_match_cmp(a, value, |o| o == Ordering::Greater)
            }),
            Filter::Gte { field, value } => with_attr(attrs, field, |a| {
                leaf_match_cmp(a, value, |o| o != Ordering::Less)
            }),
            Filter::Lt { field, value } => with_attr(attrs, field, |a| {
                leaf_match_cmp(a, value, |o| o == Ordering::Less)
            }),
            Filter::Lte { field, value } => with_attr(attrs, field, |a| {
                leaf_match_cmp(a, value, |o| o != Ordering::Greater)
            }),
            Filter::In { field, values } | Filter::ContainsAny { field, values } => {
                with_attr(attrs, field, |a| {
                    values.iter().any(|v| leaf_match_eq(a, v))
                })
            }
            Filter::StartsWith { field, value } => with_attr(attrs, field, |a| {
                leaf_match_str(a, |s| s.starts_with(value.as_str()))
            }),
            Filter::Contains { field, value } => with_attr(attrs, field, |a| {
                leaf_match_str(a, |s| s.contains(value.as_str()))
            }),
        }
    }
}

/// Applies `pred` to the attribute if it is present and non-null; otherwise
/// the leaf is `false`.
fn with_attr(attrs: &Attributes, field: &str, pred: impl Fn(&Value) -> bool) -> bool {
    match attrs.get(field) {
        None | Some(Value::Null) => false,
        Some(v) => pred(v),
    }
}

/// Equality with "any element" semantics for array attributes.
fn leaf_match_eq(attr: &Value, target: &Value) -> bool {
    match attr {
        Value::Array(items) => items.iter().any(|it| scalar_eq(it, target)),
        other => scalar_eq(other, target),
    }
}

/// Scalar equality: bools, strings, and cross-type numeric equality.
/// Arrays and nulls on either side never compare equal.
fn scalar_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        _ => match (as_num(a), as_num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// Ordering with "any element" semantics for array attributes.
fn leaf_match_cmp(attr: &Value, target: &Value, pred: impl Fn(Ordering) -> bool) -> bool {
    match attr {
        Value::Array(items) => items
            .iter()
            .any(|it| scalar_cmp(it, target).is_some_and(&pred)),
        other => scalar_cmp(other, target).is_some_and(&pred),
    }
}

/// Scalar ordering: numbers compare numerically (cross-type), strings
/// lexicographically. Everything else (bools, arrays, NaN) is incomparable.
fn scalar_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        _ => {
            let x = as_num(a)?;
            let y = as_num(b)?;
            x.partial_cmp(&y)
        }
    }
}

/// String predicate with "any element" semantics for arrays of strings.
fn leaf_match_str(attr: &Value, pred: impl Fn(&str) -> bool) -> bool {
    match attr {
        Value::Str(s) => pred(s),
        Value::Array(items) => items
            .iter()
            .any(|it| it.as_str().map(&pred).unwrap_or(false)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(pairs: Vec<(&str, Value)>) -> Attributes {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn eq_and_cross_type_numeric() {
        let d = doc(vec![("n", Value::Int(5)), ("s", Value::from("abc"))]);
        assert!(Filter::eq("n", 5i64).matches(&d));
        assert!(Filter::eq("n", 5.0).matches(&d));
        assert!(!Filter::eq("n", 6i64).matches(&d));
        assert!(Filter::eq("s", "abc").matches(&d));
        assert!(!Filter::eq("s", "ab").matches(&d));
    }

    #[test]
    fn missing_and_null_fields_never_match_leaves() {
        let d = doc(vec![("x", Value::Null)]);
        for f in [
            Filter::eq("x", 1i64),
            Filter::not_eq("x", 1i64),
            Filter::gt("x", 0i64),
            Filter::eq("absent", 1i64),
            Filter::not_eq("absent", 1i64),
            Filter::starts_with("absent", "a"),
        ] {
            assert!(!f.matches(&d), "{f:?} should not match");
        }
        // But Not(Eq) does match a document lacking the field.
        assert!(Filter::eq("absent", 1i64).negate().matches(&d));
    }

    #[test]
    fn array_any_element_semantics() {
        let d = doc(vec![(
            "tags",
            Value::Array(vec!["a".into(), "b".into(), Value::Int(3)]),
        )]);
        assert!(Filter::eq("tags", "a").matches(&d));
        assert!(Filter::eq("tags", 3i64).matches(&d));
        assert!(!Filter::eq("tags", "c").matches(&d));
        assert!(Filter::contains_any("tags", ["c", "b"]).matches(&d));
        assert!(Filter::gt("tags", 2i64).matches(&d)); // element 3 > 2
        assert!(Filter::starts_with("tags", "a").matches(&d));
    }

    #[test]
    fn range_semantics() {
        let d = doc(vec![("score", Value::Float(7.5)), ("name", Value::from("mango"))]);
        assert!(Filter::gt("score", 7i64).matches(&d));
        assert!(Filter::gte("score", 7.5).matches(&d));
        assert!(!Filter::gt("score", 7.5).matches(&d));
        assert!(Filter::lt("score", 8i64).matches(&d));
        assert!(Filter::lte("score", 7.5).matches(&d));
        assert!(Filter::gt("name", "apple").matches(&d));
        assert!(Filter::lt("name", "zebra").matches(&d));
    }

    #[test]
    fn booleans_are_not_ordered() {
        let d = doc(vec![("active", Value::Bool(true))]);
        assert!(Filter::eq("active", true).matches(&d));
        assert!(!Filter::gt("active", false).matches(&d));
        assert!(!Filter::lt("active", true).matches(&d));
    }

    #[test]
    fn nan_never_matches() {
        let d = doc(vec![("x", Value::Float(f64::NAN))]);
        assert!(!Filter::eq("x", f64::NAN).matches(&d));
        assert!(!Filter::gt("x", 0.0).matches(&d));
        assert!(!Filter::lt("x", 0.0).matches(&d));
        // NotEq: field is present, equality is false -> NotEq matches.
        assert!(Filter::not_eq("x", 1.0).matches(&d));
    }

    #[test]
    fn boolean_combinators() {
        let d = doc(vec![("a", Value::Int(1)), ("b", Value::Int(2))]);
        assert!(Filter::and(vec![]).matches(&d));
        assert!(!Filter::or(vec![]).matches(&d));
        assert!(Filter::and(vec![Filter::eq("a", 1i64), Filter::eq("b", 2i64)]).matches(&d));
        assert!(!Filter::and(vec![Filter::eq("a", 1i64), Filter::eq("b", 3i64)]).matches(&d));
        assert!(Filter::or(vec![Filter::eq("a", 9i64), Filter::eq("b", 2i64)]).matches(&d));
        assert!(Filter::eq("a", 9i64).negate().matches(&d));
    }
}
