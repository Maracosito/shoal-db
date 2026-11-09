use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

use crate::types::{AttributeValue, Document};

/// Metadata filter expression tree.
///
/// JSON form is externally tagged, e.g.:
/// `{"and":[{"eq":{"field":"lang","value":"en"}},{"gte":{"field":"stars","value":10}}]}`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    Eq { field: String, value: AttributeValue },
    NotEq { field: String, value: AttributeValue },
    Gt { field: String, value: AttributeValue },
    Gte { field: String, value: AttributeValue },
    Lt { field: String, value: AttributeValue },
    Lte { field: String, value: AttributeValue },
    In { field: String, values: Vec<AttributeValue> },
    ContainsAny { field: String, values: Vec<String> },
    /// Glob string match supporting `*` (any run) and `?` (one char).
    Glob { field: String, pattern: String },
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}

/// Compare two attribute values where it makes sense. Int/Float compare
/// numerically; strings lexically; bools false < true. Returns `None` for
/// incomparable types.
pub fn cmp_values(a: &AttributeValue, b: &AttributeValue) -> Option<Ordering> {
    use AttributeValue::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Equality with numeric Int/Float coercion and structural list equality.
pub fn values_eq(a: &AttributeValue, b: &AttributeValue) -> bool {
    use AttributeValue::*;
    match (a, b) {
        (StringList(x), StringList(y)) => x == y,
        (Null, Null) => true,
        _ => cmp_values(a, b) == Some(Ordering::Equal),
    }
}

/// Glob match with `*` and `?`, iterative with backtracking.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl Filter {
    /// Evaluate this filter against a document. The pseudo-field `"id"`
    /// resolves to the document id as a string.
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Filter::And(fs) => fs.iter().all(|f| f.matches(doc)),
            Filter::Or(fs) => fs.iter().any(|f| f.matches(doc)),
            Filter::Not(f) => !f.matches(doc),
            Filter::Eq { field, value } => {
                Self::lookup(doc, field).map_or(false, |v| values_eq(&v, value))
            }
            Filter::NotEq { field, value } => {
                Self::lookup(doc, field).map_or(true, |v| !values_eq(&v, value))
            }
            Filter::Gt { field, value } => Self::lookup(doc, field)
                .and_then(|v| cmp_values(&v, value))
                .map_or(false, |o| o == Ordering::Greater),
            Filter::Gte { field, value } => Self::lookup(doc, field)
                .and_then(|v| cmp_values(&v, value))
                .map_or(false, |o| o != Ordering::Less),
            Filter::Lt { field, value } => Self::lookup(doc, field)
                .and_then(|v| cmp_values(&v, value))
                .map_or(false, |o| o == Ordering::Less),
            Filter::Lte { field, value } => Self::lookup(doc, field)
                .and_then(|v| cmp_values(&v, value))
                .map_or(false, |o| o != Ordering::Greater),
            Filter::In { field, values } => Self::lookup(doc, field)
                .map_or(false, |v| values.iter().any(|w| values_eq(&v, w))),
            Filter::ContainsAny { field, values } => match Self::lookup(doc, field) {
                Some(AttributeValue::String(s)) => values.iter().any(|v| v == &s),
                Some(AttributeValue::StringList(l)) => {
                    values.iter().any(|v| l.iter().any(|e| e == v))
                }
                _ => false,
            },
            Filter::Glob { field, pattern } => match Self::lookup(doc, field) {
                Some(AttributeValue::String(s)) => glob_match(pattern, &s),
                Some(AttributeValue::StringList(l)) => {
                    l.iter().any(|s| glob_match(pattern, s))
                }
                _ => false,
            },
        }
    }

    fn lookup(doc: &Document, field: &str) -> Option<AttributeValue> {
        if field == "id" {
            return Some(AttributeValue::String(doc.id.clone()));
        }
        doc.attributes.get(field).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc() -> Document {
        let mut attributes = BTreeMap::new();
        attributes.insert("lang".into(), AttributeValue::String("en".into()));
        attributes.insert("stars".into(), AttributeValue::Int(12));
        attributes.insert(
            "tags".into(),
            AttributeValue::StringList(vec!["rust".into(), "db".into()]),
        );
        Document {
            id: "doc-1".into(),
            vector: None,
            sparse_vector: None,
            attributes,
        }
    }

    #[test]
    fn eq_and_numeric_coercion() {
        let d = doc();
        assert!(Filter::Eq {
            field: "stars".into(),
            value: AttributeValue::Float(12.0)
        }
        .matches(&d));
        assert!(Filter::Gte {
            field: "stars".into(),
            value: AttributeValue::Int(12)
        }
        .matches(&d));
        assert!(!Filter::Gt {
            field: "stars".into(),
            value: AttributeValue::Int(12)
        }
        .matches(&d));
    }

    #[test]
    fn boolean_combinators_and_id() {
        let d = doc();
        let f = Filter::And(vec![
            Filter::Eq {
                field: "lang".into(),
                value: AttributeValue::String("en".into()),
            },
            Filter::Not(Box::new(Filter::Eq {
                field: "id".into(),
                value: AttributeValue::String("other".into()),
            })),
        ]);
        assert!(f.matches(&d));
    }

    #[test]
    fn contains_any_and_glob() {
        let d = doc();
        assert!(Filter::ContainsAny {
            field: "tags".into(),
            values: vec!["db".into(), "zzz".into()]
        }
        .matches(&d));
        assert!(Filter::Glob {
            field: "lang".into(),
            pattern: "e*".into()
        }
        .matches(&d));
        assert!(glob_match("a*c?e", "abbbcde"));
        assert!(!glob_match("a*c?e", "abbbce"));
    }
}
