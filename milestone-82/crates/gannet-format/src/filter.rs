//! The metadata filter AST, mirroring the `Filter` schema in
//! `api/openapi.yaml`.
//!
//! Wire shape: every node is a JSON object with an `op` discriminator, e.g.
//!
//! ```json
//! {"op": "and", "filters": [
//!   {"op": "eq", "attr": "genre", "value": "sci-fi"},
//!   {"op": "gte", "attr": "year", "value": 1990},
//!   {"op": "not", "filter": {"op": "contains_any", "attr": "tags",
//!                            "values": ["draft", "archived"]}}
//! ]}
//! ```
//!
//! ## Semantics (normative)
//!
//! * All **leaf predicates evaluate to `false` when the attribute is
//!   missing** — including `not_eq`. To express "missing or different",
//!   write `{"op": "not", "filter": {"op": "eq", ...}}`.
//! * Numeric comparisons coerce between integers and floats; values of
//!   incomparable types make range predicates `false`.
//! * `in`: a scalar attribute matches if it equals any listed value; an
//!   array attribute matches if any element does.
//! * `contains_any`: identical membership semantics; provided as a distinct
//!   op because it is the idiomatic spelling for array attributes.
//! * `contains` / `starts_with`: substring / prefix match on string
//!   attributes (or any string element of an array attribute).
//! * Empty `and` is `true`; empty `or` is `false`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::document::AttributeValue;

/// A filter expression tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Filter {
    Eq { attr: String, value: AttributeValue },
    NotEq { attr: String, value: AttributeValue },
    Gt { attr: String, value: AttributeValue },
    Gte { attr: String, value: AttributeValue },
    Lt { attr: String, value: AttributeValue },
    Lte { attr: String, value: AttributeValue },
    In { attr: String, values: Vec<AttributeValue> },
    ContainsAny { attr: String, values: Vec<AttributeValue> },
    Contains { attr: String, value: String },
    StartsWith { attr: String, value: String },
    And { filters: Vec<Filter> },
    Or { filters: Vec<Filter> },
    Not { filter: Box<Filter> },
}

/// Structural validation failure for a filter.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    #[error("filter nesting depth {depth} exceeds maximum {max}")]
    TooDeep { depth: usize, max: usize },
    #[error("filter leaf has an empty attribute name")]
    EmptyAttributeName,
}

type Attrs = BTreeMap<String, AttributeValue>;

fn cmp_pred(attrs: &Attrs, attr: &str, value: &AttributeValue, pred: fn(std::cmp::Ordering) -> bool) -> bool {
    attrs.get(attr).and_then(|v| v.compare(value)).is_some_and(pred)
}

fn membership(attrs: &Attrs, attr: &str, values: &[AttributeValue]) -> bool {
    match attrs.get(attr) {
        Some(AttributeValue::Array(items)) => items
            .iter()
            .any(|item| values.iter().any(|v| item.loosely_equals(v))),
        Some(scalar) => values.iter().any(|v| scalar.loosely_equals(v)),
        None => false,
    }
}

fn string_pred(attrs: &Attrs, attr: &str, f: impl Fn(&str) -> bool) -> bool {
    match attrs.get(attr) {
        Some(AttributeValue::String(s)) => f(s),
        Some(AttributeValue::Array(items)) => items
            .iter()
            .any(|i| matches!(i, AttributeValue::String(s) if f(s))),
        _ => false,
    }
}

impl Filter {
    /// Maximum permitted nesting depth, enforced by [`Filter::validate`].
    pub const MAX_DEPTH: usize = 32;

    /// Evaluate this filter against a document's attributes.
    ///
    /// This is the reference (and exact-scan) implementation; index-backed
    /// execution in the engine must produce identical results.
    pub fn evaluate(&self, attrs: &Attrs) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Filter::Eq { attr, value } => {
                attrs.get(attr).is_some_and(|v| v.loosely_equals(value))
            }
            Filter::NotEq { attr, value } => {
                attrs.get(attr).is_some_and(|v| !v.loosely_equals(value))
            }
            Filter::Gt { attr, value } => cmp_pred(attrs, attr, value, |o| o == Greater),
            Filter::Gte { attr, value } => cmp_pred(attrs, attr, value, |o| o != Less),
            Filter::Lt { attr, value } => cmp_pred(attrs, attr, value, |o| o == Less),
            Filter::Lte { attr, value } => cmp_pred(attrs, attr, value, |o| o != Greater),
            Filter::In { attr, values } => membership(attrs, attr, values),
            Filter::ContainsAny { attr, values } => membership(attrs, attr, values),
            Filter::Contains { attr, value } => {
                string_pred(attrs, attr, |s| s.contains(value.as_str()))
            }
            Filter::StartsWith { attr, value } => {
                string_pred(attrs, attr, |s| s.starts_with(value.as_str()))
            }
            Filter::And { filters } => filters.iter().all(|f| f.evaluate(attrs)),
            Filter::Or { filters } => filters.iter().any(|f| f.evaluate(attrs)),
            Filter::Not { filter } => !filter.evaluate(attrs),
        }
    }

    /// Nesting depth of this expression (a leaf has depth 1).
    pub fn depth(&self) -> usize {
        match self {
            Filter::And { filters } | Filter::Or { filters } => {
                1 + filters.iter().map(Filter::depth).max().unwrap_or(0)
            }
            Filter::Not { filter } => 1 + filter.depth(),
            _ => 1,
        }
    }

    /// Structural validation: bounded depth, non-empty attribute names.
    pub fn validate(&self) -> Result<(), FilterError> {
        let depth = self.depth();
        if depth > Self::MAX_DEPTH {
            return Err(FilterError::TooDeep {
                depth,
                max: Self::MAX_DEPTH,
            });
        }
        self.walk_validate()
    }

    fn walk_validate(&self) -> Result<(), FilterError> {
        match self {
            Filter::And { filters } | Filter::Or { filters } => {
                for f in filters {
                    f.walk_validate()?;
                }
                Ok(())
            }
            Filter::Not { filter } => filter.walk_validate(),
            leaf => {
                if leaf.leaf_attr().is_some_and(str::is_empty) {
                    Err(FilterError::EmptyAttributeName)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn leaf_attr(&self) -> Option<&str> {
        match self {
            Filter::Eq { attr, .. }
            | Filter::NotEq { attr, .. }
            | Filter::Gt { attr, .. }
            | Filter::Gte { attr, .. }
            | Filter::Lt { attr, .. }
            | Filter::Lte { attr, .. }
            | Filter::In { attr, .. }
            | Filter::ContainsAny { attr, .. }
            | Filter::Contains { attr, .. }
            | Filter::StartsWith { attr, .. } => Some(attr),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(json: &str) -> Attrs {
        serde_json::from_str(json).unwrap()
    }

    fn filter(json: &str) -> Filter {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn wire_format_round_trip() {
        let f = filter(
            r#"{"op":"and","filters":[
                {"op":"eq","attr":"genre","value":"sci-fi"},
                {"op":"gte","attr":"year","value":1990},
                {"op":"not","filter":{"op":"in","attr":"status","values":["draft"]}}
            ]}"#,
        );
        let json = serde_json::to_string(&f).unwrap();
        let back: Filter = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
        assert!(json.contains("\"op\":\"and\""));
        assert!(json.contains("\"op\":\"gte\""));
    }

    #[test]
    fn eq_and_not_eq() {
        let a = attrs(r#"{"genre": "sci-fi", "year": 1990}"#);
        assert!(filter(r#"{"op":"eq","attr":"genre","value":"sci-fi"}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"eq","attr":"genre","value":"romance"}"#).evaluate(&a));
        // Missing attribute: both eq and not_eq are false.
        assert!(!filter(r#"{"op":"eq","attr":"missing","value":1}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"not_eq","attr":"missing","value":1}"#).evaluate(&a));
        // not(eq) is true on missing.
        assert!(
            filter(r#"{"op":"not","filter":{"op":"eq","attr":"missing","value":1}}"#).evaluate(&a)
        );
        // Numeric coercion: 1990 == 1990.0
        assert!(filter(r#"{"op":"eq","attr":"year","value":1990.0}"#).evaluate(&a));
    }

    #[test]
    fn range_operators() {
        let a = attrs(r#"{"year": 1990, "score": 0.75}"#);
        assert!(filter(r#"{"op":"gt","attr":"year","value":1989}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"gt","attr":"year","value":1990}"#).evaluate(&a));
        assert!(filter(r#"{"op":"gte","attr":"year","value":1990}"#).evaluate(&a));
        assert!(filter(r#"{"op":"lt","attr":"score","value":0.8}"#).evaluate(&a));
        assert!(filter(r#"{"op":"lte","attr":"score","value":0.75}"#).evaluate(&a));
        // Incomparable types are false.
        assert!(!filter(r#"{"op":"gt","attr":"year","value":"banana"}"#).evaluate(&a));
        // Strings compare lexicographically.
        let b = attrs(r#"{"name": "mango"}"#);
        assert!(filter(r#"{"op":"gt","attr":"name","value":"apple"}"#).evaluate(&b));
    }

    #[test]
    fn membership_operators() {
        let a = attrs(r#"{"genre": "sci-fi", "tags": ["alpha", "beta"]}"#);
        assert!(filter(r#"{"op":"in","attr":"genre","values":["sci-fi","fantasy"]}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"in","attr":"genre","values":["fantasy"]}"#).evaluate(&a));
        assert!(
            filter(r#"{"op":"contains_any","attr":"tags","values":["beta","gamma"]}"#).evaluate(&a)
        );
        assert!(
            !filter(r#"{"op":"contains_any","attr":"tags","values":["gamma"]}"#).evaluate(&a)
        );
        // `in` on an array attribute behaves like contains_any.
        assert!(filter(r#"{"op":"in","attr":"tags","values":["alpha"]}"#).evaluate(&a));
    }

    #[test]
    fn string_matching() {
        let a = attrs(r#"{"title": "object storage native search", "paths": ["src/lib.rs"]}"#);
        assert!(filter(r#"{"op":"contains","attr":"title","value":"storage"}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"contains","attr":"title","value":"graph"}"#).evaluate(&a));
        assert!(filter(r#"{"op":"starts_with","attr":"title","value":"object"}"#).evaluate(&a));
        assert!(filter(r#"{"op":"starts_with","attr":"paths","value":"src/"}"#).evaluate(&a));
        // Non-string attributes never match string predicates.
        let b = attrs(r#"{"n": 42}"#);
        assert!(!filter(r#"{"op":"contains","attr":"n","value":"4"}"#).evaluate(&b));
    }

    #[test]
    fn boolean_combinators() {
        let a = attrs(r#"{"genre": "sci-fi", "year": 1990}"#);
        assert!(filter(
            r#"{"op":"and","filters":[
                {"op":"eq","attr":"genre","value":"sci-fi"},
                {"op":"gte","attr":"year","value":1990}
            ]}"#
        )
        .evaluate(&a));
        assert!(filter(
            r#"{"op":"or","filters":[
                {"op":"eq","attr":"genre","value":"romance"},
                {"op":"eq","attr":"genre","value":"sci-fi"}
            ]}"#
        )
        .evaluate(&a));
        // Empty and/or identities.
        assert!(filter(r#"{"op":"and","filters":[]}"#).evaluate(&a));
        assert!(!filter(r#"{"op":"or","filters":[]}"#).evaluate(&a));
    }

    #[test]
    fn validation() {
        assert!(filter(r#"{"op":"eq","attr":"x","value":1}"#).validate().is_ok());
        assert_eq!(
            filter(r#"{"op":"eq","attr":"","value":1}"#).validate(),
            Err(FilterError::EmptyAttributeName)
        );
        // Build a chain deeper than MAX_DEPTH.
        let mut f = Filter::Eq {
            attr: "x".into(),
            value: AttributeValue::Int(1),
        };
        for _ in 0..Filter::MAX_DEPTH {
            f = Filter::Not { filter: Box::new(f) };
        }
        assert!(matches!(f.validate(), Err(FilterError::TooDeep { .. })));
    }
}
