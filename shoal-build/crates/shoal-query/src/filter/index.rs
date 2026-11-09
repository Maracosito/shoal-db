//! Persistent attribute index for set-based filter evaluation.
//!
//! The [`AttributeIndex`] maps, per field, each indexed value to the
//! [`DocSet`] of documents carrying it:
//!
//! * booleans and strings are stored in ordered dictionaries
//!   (`BTreeMap<_, DocSet>`), enabling exact lookups, prefix scans, and
//!   lexicographic ranges;
//! * numbers (ints widened to `f64`) are keyed by a total-order encoding of
//!   the float's bits, so `Gt`/`Gte`/`Lt`/`Lte` become `BTreeMap::range`
//!   scans; `NaN` is never indexed, and `-0.0` is normalized to `0.0`;
//! * a per-field `present` set supports `NotEq` (present ∧ ¬equal);
//! * the segment-wide `universe` supports `Not` (complement).
//!
//! Array attributes are indexed element-wise, matching the "any element"
//! row-evaluation semantics in [`super`]. The index is an immutable artifact:
//! it is built once per segment, serialized with the [`crate::codec`]
//! primitives (magic `SATR`), uploaded to object storage next to the segment,
//! and cached on local disk by the serving tier.
//!
//! Precision note: `i64` values outside ±2^53 lose precision when widened to
//! `f64`. This matches the row-evaluation semantics (which also compare via
//! `f64`), so the two paths still agree; applications needing exact huge-int
//! keys should store them as strings.

use super::docset::DocSet;
use super::{as_num, Attributes, Filter, Value};
use crate::codec::{CodecError, Reader, Writer};
use std::collections::BTreeMap;
use std::ops::Bound;

/// `f64` wrapper with a total order suitable for `BTreeMap` keys.
///
/// The key transform maps the IEEE-754 bit pattern to an unsigned integer
/// whose natural order equals numeric order: negative floats have all bits
/// inverted; non-negative floats have the sign bit set.
#[derive(Clone, Copy, Debug)]
struct OrderedF64(f64);

impl OrderedF64 {
    /// Rejects `NaN`; normalizes `-0.0` to `0.0`.
    fn new(v: f64) -> Option<Self> {
        if v.is_nan() {
            None
        } else if v == 0.0 {
            Some(OrderedF64(0.0))
        } else {
            Some(OrderedF64(v))
        }
    }

    fn key(self) -> u64 {
        let bits = self.0.to_bits();
        if bits & (1u64 << 63) != 0 {
            !bits
        } else {
            bits | (1u64 << 63)
        }
    }

    fn value(self) -> f64 {
        self.0
    }
}

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// Per-field value dictionaries.
#[derive(Debug, Default)]
struct FieldIndex {
    /// Documents where the field is present and non-null.
    present: DocSet,
    bools: BTreeMap<bool, DocSet>,
    nums: BTreeMap<OrderedF64, DocSet>,
    strs: BTreeMap<String, DocSet>,
}

/// Incremental builder for an [`AttributeIndex`].
///
/// Feed each document exactly once via [`add_doc`](Self::add_doc) (ids may
/// arrive in any order), then call [`build`](Self::build).
#[derive(Debug, Default)]
pub struct AttributeIndexBuilder {
    docs: Vec<u32>,
    fields: BTreeMap<String, FieldAccum>,
}

#[derive(Debug, Default)]
struct FieldAccum {
    present: Vec<u32>,
    bools: BTreeMap<bool, Vec<u32>>,
    nums: BTreeMap<OrderedF64, Vec<u32>>,
    strs: BTreeMap<String, Vec<u32>>,
}

impl AttributeIndexBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes one document's attributes under `doc_id`.
    pub fn add_doc(&mut self, doc_id: u32, attrs: &Attributes) {
        self.docs.push(doc_id);
        for (name, value) in attrs {
            if matches!(value, Value::Null) {
                continue;
            }
            let acc = self.fields.entry(name.clone()).or_default();
            acc.present.push(doc_id);
            Self::index_value(acc, doc_id, value);
        }
    }

    fn index_value(acc: &mut FieldAccum, doc_id: u32, value: &Value) {
        match value {
            Value::Bool(b) => acc.bools.entry(*b).or_default().push(doc_id),
            Value::Str(s) => acc.strs.entry(s.clone()).or_default().push(doc_id),
            Value::Int(_) | Value::Float(_) => {
                if let Some(n) = as_num(value) {
                    if let Some(k) = OrderedF64::new(n) {
                        acc.nums.entry(k).or_default().push(doc_id);
                    }
                }
            }
            Value::Array(items) => {
                for it in items {
                    // Nested arrays and nulls inside arrays are not indexed,
                    // matching row-evaluation semantics.
                    if !matches!(it, Value::Array(_) | Value::Null) {
                        Self::index_value(acc, doc_id, it);
                    }
                }
            }
            Value::Null => {}
        }
    }

    /// Finalizes the index.
    pub fn build(self) -> AttributeIndex {
        let to_sets = |m: BTreeMap<bool, Vec<u32>>| -> BTreeMap<bool, DocSet> {
            m.into_iter()
                .map(|(k, v)| (k, DocSet::from_unsorted(v)))
                .collect()
        };
        AttributeIndex {
            universe: DocSet::from_unsorted(self.docs),
            fields: self
                .fields
                .into_iter()
                .map(|(name, acc)| {
                    (
                        name,
                        FieldIndex {
                            present: DocSet::from_unsorted(acc.present),
                            bools: to_sets(acc.bools),
                            nums: acc
                                .nums
                                .into_iter()
                                .map(|(k, v)| (k, DocSet::from_unsorted(v)))
                                .collect(),
                            strs: acc
                                .strs
                                .into_iter()
                                .map(|(k, v)| (k, DocSet::from_unsorted(v)))
                                .collect(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Clone, Copy)]
enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

/// Immutable, serializable attribute index over one segment.
#[derive(Debug)]
pub struct AttributeIndex {
    universe: DocSet,
    fields: BTreeMap<String, FieldIndex>,
}

impl AttributeIndex {
    /// Magic bytes identifying a serialized attribute index ("Shoal ATtRibute").
    pub const MAGIC: [u8; 4] = *b"SATR";
    /// Current format version.
    pub const VERSION: u32 = 1;

    /// Number of documents covered by this index.
    pub fn doc_count(&self) -> usize {
        self.universe.len()
    }

    /// The set of all documents covered by this index.
    pub fn universe(&self) -> &DocSet {
        &self.universe
    }

    /// Names of all indexed fields.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }

    /// Evaluates a filter to the exact set of matching documents.
    ///
    /// Agrees with [`Filter::matches`] applied row-wise over the indexed
    /// documents (see `tests/filter_algebra.rs`).
    pub fn eval(&self, filter: &Filter) -> DocSet {
        match filter {
            Filter::And { filters } => {
                if filters.is_empty() {
                    return self.universe.clone();
                }
                let mut sets: Vec<DocSet> = filters.iter().map(|f| self.eval(f)).collect();
                // Intersect smallest-first to shrink intermediates quickly.
                sets.sort_by_key(DocSet::len);
                let mut iter = sets.into_iter();
                let mut acc = iter.next().expect("non-empty");
                for s in iter {
                    if acc.is_empty() {
                        break;
                    }
                    acc = acc.intersect(&s);
                }
                acc
            }
            Filter::Or { filters } => filters
                .iter()
                .fold(DocSet::empty(), |acc, f| acc.union(&self.eval(f))),
            Filter::Not { filter } => self.universe.difference(&self.eval(filter)),
            Filter::Eq { field, value } => self.eval_eq(field, value),
            Filter::NotEq { field, value } => match self.fields.get(field) {
                Some(fi) => fi.present.difference(&self.eval_eq(field, value)),
                None => DocSet::empty(),
            },
            Filter::Gt { field, value } => self.eval_cmp(field, value, CmpOp::Gt),
            Filter::Gte { field, value } => self.eval_cmp(field, value, CmpOp::Gte),
            Filter::Lt { field, value } => self.eval_cmp(field, value, CmpOp::Lt),
            Filter::Lte { field, value } => self.eval_cmp(field, value, CmpOp::Lte),
            Filter::In { field, values } | Filter::ContainsAny { field, values } => values
                .iter()
                .fold(DocSet::empty(), |acc, v| acc.union(&self.eval_eq(field, v))),
            Filter::StartsWith { field, value } => self.eval_prefix(field, value),
            Filter::Contains { field, value } => self.eval_substring(field, value),
        }
    }

    fn eval_eq(&self, field: &str, value: &Value) -> DocSet {
        let Some(fi) = self.fields.get(field) else {
            return DocSet::empty();
        };
        match value {
            Value::Bool(b) => fi.bools.get(b).cloned().unwrap_or_default(),
            Value::Str(s) => fi.strs.get(s).cloned().unwrap_or_default(),
            Value::Int(_) | Value::Float(_) => match as_num(value).and_then(OrderedF64::new) {
                Some(k) => fi.nums.get(&k).cloned().unwrap_or_default(),
                None => DocSet::empty(),
            },
            // Equality against null/array literals never matches; use
            // In/ContainsAny for membership semantics.
            Value::Null | Value::Array(_) => DocSet::empty(),
        }
    }

    fn eval_cmp(&self, field: &str, value: &Value, op: CmpOp) -> DocSet {
        let Some(fi) = self.fields.get(field) else {
            return DocSet::empty();
        };
        match value {
            Value::Str(s) => {
                let bounds: (Bound<&str>, Bound<&str>) = match op {
                    CmpOp::Gt => (Bound::Excluded(s.as_str()), Bound::Unbounded),
                    CmpOp::Gte => (Bound::Included(s.as_str()), Bound::Unbounded),
                    CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(s.as_str())),
                    CmpOp::Lte => (Bound::Unbounded, Bound::Included(s.as_str())),
                };
                fi.strs
                    .range::<str, _>(bounds)
                    .fold(DocSet::empty(), |acc, (_, set)| acc.union(set))
            }
            _ => {
                let Some(k) = as_num(value).and_then(OrderedF64::new) else {
                    return DocSet::empty();
                };
                let bounds = match op {
                    CmpOp::Gt => (Bound::Excluded(k), Bound::Unbounded),
                    CmpOp::Gte => (Bound::Included(k), Bound::Unbounded),
                    CmpOp::Lt => (Bound::Unbounded, Bound::Excluded(k)),
                    CmpOp::Lte => (Bound::Unbounded, Bound::Included(k)),
                };
                fi.nums
                    .range(bounds)
                    .fold(DocSet::empty(), |acc, (_, set)| acc.union(set))
            }
        }
    }

    fn eval_prefix(&self, field: &str, prefix: &str) -> DocSet {
        let Some(fi) = self.fields.get(field) else {
            return DocSet::empty();
        };
        let mut acc = DocSet::empty();
        for (k, set) in fi
            .strs
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
        {
            if !k.starts_with(prefix) {
                break;
            }
            acc = acc.union(set);
        }
        acc
    }

    fn eval_substring(&self, field: &str, needle: &str) -> DocSet {
        let Some(fi) = self.fields.get(field) else {
            return DocSet::empty();
        };
        // Substring matching scans the term dictionary (O(distinct terms)).
        // The planner treats Contains as expensive accordingly.
        fi.strs
            .iter()
            .filter(|(k, _)| k.contains(needle))
            .fold(DocSet::empty(), |acc, (_, set)| acc.union(set))
    }

    /// Cheap cardinality estimate used by the query planner to choose between
    /// pre-filtered exact search and post-filtered ANN.
    ///
    /// For positive leaves (`Eq`, `In`, ranges, prefixes), `And` and `Or`,
    /// the estimate is an upper bound on the true cardinality. Estimates
    /// involving `Not`/`NotEq` are heuristic, not bounds.
    pub fn estimate(&self, filter: &Filter) -> usize {
        let n = self.universe.len();
        match filter {
            Filter::And { filters } => filters
                .iter()
                .map(|f| self.estimate(f))
                .min()
                .unwrap_or(n),
            Filter::Or { filters } => filters
                .iter()
                .map(|f| self.estimate(f))
                .sum::<usize>()
                .min(n),
            Filter::Not { filter } => n.saturating_sub(self.estimate(filter)),
            Filter::NotEq { field, .. } => self
                .fields
                .get(field)
                .map(|fi| fi.present.len())
                .unwrap_or(0),
            _ => self.eval(filter).len(),
        }
    }

    /// Serializes the index (magic `SATR`, version 1).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(64 + self.universe.len() * 2);
        w.put_header(&Self::MAGIC, Self::VERSION);
        self.universe.encode(&mut w);
        w.put_varint(self.fields.len() as u64);
        for (name, fi) in &self.fields {
            w.put_str(name);
            fi.present.encode(&mut w);
            w.put_varint(fi.bools.len() as u64);
            for (k, set) in &fi.bools {
                w.put_u8(*k as u8);
                set.encode(&mut w);
            }
            w.put_varint(fi.nums.len() as u64);
            for (k, set) in &fi.nums {
                w.put_f64(k.value());
                set.encode(&mut w);
            }
            w.put_varint(fi.strs.len() as u64);
            for (k, set) in &fi.strs {
                w.put_str(k);
                set.encode(&mut w);
            }
        }
        w.into_bytes()
    }

    /// Deserializes an index written by [`encode`](Self::encode).
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(bytes);
        r.check_header(&Self::MAGIC, Self::VERSION)?;
        let universe = DocSet::decode(&mut r)?;
        let nfields = r.get_varint()? as usize;
        let mut fields = BTreeMap::new();
        for _ in 0..nfields {
            let name = r.get_str()?;
            let present = DocSet::decode(&mut r)?;

            let nbools = r.get_varint()? as usize;
            let mut bools = BTreeMap::new();
            for _ in 0..nbools {
                let k = match r.get_u8()? {
                    0 => false,
                    1 => true,
                    other => {
                        return Err(CodecError::Corrupt(format!(
                            "invalid bool key byte {other}"
                        )))
                    }
                };
                bools.insert(k, DocSet::decode(&mut r)?);
            }

            let nnums = r.get_varint()? as usize;
            let mut nums = BTreeMap::new();
            for _ in 0..nnums {
                let f = r.get_f64()?;
                let k = OrderedF64::new(f).ok_or_else(|| {
                    CodecError::Corrupt("NaN numeric key in attribute index".to_string())
                })?;
                nums.insert(k, DocSet::decode(&mut r)?);
            }

            let nstrs = r.get_varint()? as usize;
            let mut strs = BTreeMap::new();
            for _ in 0..nstrs {
                let k = r.get_str()?;
                strs.insert(k, DocSet::decode(&mut r)?);
            }

            fields.insert(
                name,
                FieldIndex {
                    present,
                    bools,
                    nums,
                    strs,
                },
            );
        }
        Ok(Self { universe, fields })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(pairs: Vec<(&str, Value)>) -> Attributes {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn small_index() -> AttributeIndex {
        let docs = vec![
            doc(vec![
                ("cat", "a".into()),
                ("n", Value::Int(1)),
                ("tags", Value::Array(vec!["x".into(), "y".into()])),
            ]),
            doc(vec![
                ("cat", "b".into()),
                ("n", Value::Float(2.5)),
                ("tags", Value::Array(vec!["y".into()])),
            ]),
            doc(vec![("cat", "a".into()), ("n", Value::Int(3))]),
            doc(vec![("n", Value::Int(-7))]), // no cat, no tags
        ];
        let mut b = AttributeIndexBuilder::new();
        for (i, d) in docs.iter().enumerate() {
            b.add_doc(i as u32, d);
        }
        b.build()
    }

    #[test]
    fn eq_lookup_and_not_eq_presence() {
        let idx = small_index();
        assert_eq!(idx.eval(&Filter::eq("cat", "a")).as_slice(), &[0, 2]);
        // NotEq excludes doc 3, which lacks `cat`.
        assert_eq!(idx.eval(&Filter::not_eq("cat", "a")).as_slice(), &[1]);
        // Not(Eq) includes doc 3.
        assert_eq!(
            idx.eval(&Filter::eq("cat", "a").negate()).as_slice(),
            &[1, 3]
        );
    }

    #[test]
    fn numeric_ranges_cross_type() {
        let idx = small_index();
        assert_eq!(idx.eval(&Filter::gt("n", 1i64)).as_slice(), &[1, 2]);
        assert_eq!(idx.eval(&Filter::gte("n", 2.5)).as_slice(), &[1, 2]);
        assert_eq!(idx.eval(&Filter::lt("n", 0i64)).as_slice(), &[3]);
        assert_eq!(idx.eval(&Filter::lte("n", 2.5)).as_slice(), &[0, 1, 3]);
        assert_eq!(idx.eval(&Filter::eq("n", 2.5)).as_slice(), &[1]);
    }

    #[test]
    fn array_membership() {
        let idx = small_index();
        assert_eq!(
            idx.eval(&Filter::contains_any("tags", ["x", "z"])).as_slice(),
            &[0]
        );
        assert_eq!(idx.eval(&Filter::is_in("tags", ["y"])).as_slice(), &[0, 1]);
    }

    #[test]
    fn empty_and_or_identities() {
        let idx = small_index();
        assert_eq!(idx.eval(&Filter::and(vec![])), *idx.universe());
        assert_eq!(idx.eval(&Filter::or(vec![])), DocSet::empty());
    }

    #[test]
    fn unknown_field_is_empty() {
        let idx = small_index();
        assert!(idx.eval(&Filter::eq("nope", 1i64)).is_empty());
        assert!(idx.eval(&Filter::gt("nope", 1i64)).is_empty());
        assert!(idx.eval(&Filter::not_eq("nope", 1i64)).is_empty());
        // Not over unknown field = everything.
        assert_eq!(idx.eval(&Filter::eq("nope", 1i64).negate()), *idx.universe());
    }

    #[test]
    fn encode_decode_preserves_eval() {
        let idx = small_index();
        let bytes = idx.encode();
        let back = AttributeIndex::decode(&bytes).unwrap();
        for f in [
            Filter::eq("cat", "a"),
            Filter::gt("n", 1i64),
            Filter::contains_any("tags", ["x", "y"]),
            Filter::and(vec![Filter::eq("cat", "a"), Filter::lt("n", 2i64)]),
        ] {
            assert_eq!(idx.eval(&f), back.eval(&f), "filter {f:?}");
        }
        assert_eq!(back.doc_count(), idx.doc_count());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(AttributeIndex::decode(b"XXXX\x01\x00\x00\x00").is_err());
        let bytes = small_index().encode();
        assert!(AttributeIndex::decode(&bytes[..bytes.len() - 3]).is_err());
    }
}
