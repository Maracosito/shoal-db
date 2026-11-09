//! Immutable segments.
//!
//! A segment is the unit of caching, sharing (between branches) and querying.
//! It contains:
//! - live documents (the memtable state at flush time),
//! - tombstones masking documents in *older* segments,
//! - a per-field inverted text index (postings with term frequencies and
//!   per-document field lengths) used for BM25,
//! - attribute min/max range summaries used to prune filtered queries.
//!
//! Encoding: JSON body in a CRC-checked frame (see [`crate::framing`]).
//! Segments are content-immutable: once written under a key they are never
//! modified, which is what makes copy-on-write branching safe.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::error::Result;
use crate::filter::{cmp_values, Filter};
use crate::framing;
use crate::types::{now_ms, AttributeValue, DocId, Document};
use bytes::Bytes;

pub const SEGMENT_MAGIC: &[u8; 8] = b"SHOALSG1";

/// Lowercase alphanumeric tokenizer (unicode aware).
pub fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldIndex {
    /// term -> [(doc index, term frequency)]
    pub postings: BTreeMap<String, Vec<(u32, u32)>>,
    /// per-document token count for this field (0 = field absent/empty)
    pub doc_len: Vec<u32>,
    pub total_len: u64,
    pub docs_with_field: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentData {
    pub id: String,
    pub created_at_ms: i64,
    pub min_seq: u64,
    pub max_seq: u64,
    pub docs: Vec<Document>,
    /// Doc ids deleted at flush time; they mask matching docs in older segments.
    pub tombstones: Vec<DocId>,
    /// field name -> inverted index
    pub text_index: BTreeMap<String, FieldIndex>,
    /// field name -> (min, max) over comparable scalar values, for pruning.
    pub attr_ranges: BTreeMap<String, (AttributeValue, AttributeValue)>,
}

impl SegmentData {
    pub fn build(
        id: String,
        docs: Vec<Document>,
        tombstones: Vec<DocId>,
        min_seq: u64,
        max_seq: u64,
    ) -> SegmentData {
        let mut text_index: BTreeMap<String, FieldIndex> = BTreeMap::new();
        let n = docs.len();
        for (i, d) in docs.iter().enumerate() {
            for (field, value) in &d.attributes {
                let text = match value {
                    AttributeValue::String(s) => s.clone(),
                    AttributeValue::StringList(l) => l.join(" "),
                    _ => continue,
                };
                let toks = tokenize(&text);
                if toks.is_empty() {
                    continue;
                }
                let fi = text_index.entry(field.clone()).or_insert_with(|| FieldIndex {
                    postings: BTreeMap::new(),
                    doc_len: vec![0; n],
                    total_len: 0,
                    docs_with_field: 0,
                });
                let mut tf: HashMap<String, u32> = HashMap::new();
                for t in &toks {
                    *tf.entry(t.clone()).or_insert(0) += 1;
                }
                fi.doc_len[i] = toks.len() as u32;
                fi.total_len += toks.len() as u64;
                fi.docs_with_field += 1;
                for (term, count) in tf {
                    fi.postings.entry(term).or_default().push((i as u32, count));
                }
            }
        }

        let mut attr_ranges: BTreeMap<String, (AttributeValue, AttributeValue)> = BTreeMap::new();
        let mut bad: HashSet<String> = HashSet::new();
        for d in &docs {
            for (field, value) in &d.attributes {
                if bad.contains(field) {
                    continue;
                }
                match value {
                    AttributeValue::Null | AttributeValue::StringList(_) => {
                        bad.insert(field.clone());
                        attr_ranges.remove(field);
                    }
                    _ => match attr_ranges.get_mut(field) {
                        None => {
                            attr_ranges.insert(field.clone(), (value.clone(), value.clone()));
                        }
                        Some((mn, mx)) => match (cmp_values(value, mn), cmp_values(value, mx)) {
                            (Some(o1), Some(o2)) => {
                                if o1 == std::cmp::Ordering::Less {
                                    *mn = value.clone();
                                }
                                if o2 == std::cmp::Ordering::Greater {
                                    *mx = value.clone();
                                }
                            }
                            _ => {
                                bad.insert(field.clone());
                                attr_ranges.remove(field);
                            }
                        },
                    },
                }
            }
        }

        SegmentData {
            id,
            created_at_ms: now_ms(),
            min_seq,
            max_seq,
            docs,
            tombstones,
            text_index,
            attr_ranges,
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let body = serde_json::to_vec(self)?;
        Ok(framing::encode_framed(SEGMENT_MAGIC, &body))
    }

    pub fn decode(data: &[u8]) -> Result<SegmentData> {
        let body = framing::decode_framed(SEGMENT_MAGIC, data)?;
        Ok(serde_json::from_slice(body)?)
    }

    fn range(&self, field: &str) -> Option<(&AttributeValue, &AttributeValue)> {
        self.attr_ranges.get(field).map(|(a, b)| (a, b))
    }

    /// Conservative pruning predicate: returns `false` only if NO document in
    /// this segment can satisfy the filter. Unknown/incomparable cases return
    /// `true`.
    pub fn can_match(&self, f: &Filter) -> bool {
        use std::cmp::Ordering::*;
        match f {
            Filter::And(fs) => fs.iter().all(|c| self.can_match(c)),
            Filter::Or(fs) => fs.is_empty() || fs.iter().any(|c| self.can_match(c)),
            Filter::Not(_) => true,
            Filter::Eq { field, value } => match self.range(field) {
                Some((mn, mx)) => {
                    let ge_min = cmp_values(value, mn).map_or(true, |o| o != Less);
                    let le_max = cmp_values(value, mx).map_or(true, |o| o != Greater);
                    ge_min && le_max
                }
                None => true,
            },
            Filter::In { field, values } => match self.range(field) {
                Some((mn, mx)) => values.iter().any(|value| {
                    let ge_min = cmp_values(value, mn).map_or(true, |o| o != Less);
                    let le_max = cmp_values(value, mx).map_or(true, |o| o != Greater);
                    ge_min && le_max
                }),
                None => true,
            },
            Filter::Gt { field, value } => match self.range(field) {
                Some((_, mx)) => cmp_values(mx, value).map_or(true, |o| o == Greater),
                None => true,
            },
            Filter::Gte { field, value } => match self.range(field) {
                Some((_, mx)) => cmp_values(mx, value).map_or(true, |o| o != Less),
                None => true,
            },
            Filter::Lt { field, value } => match self.range(field) {
                Some((mn, _)) => cmp_values(mn, value).map_or(true, |o| o == Less),
                None => true,
            },
            Filter::Lte { field, value } => match self.range(field) {
                Some((mn, _)) => cmp_values(mn, value).map_or(true, |o| o != Greater),
                None => true,
            },
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, text: &str, stars: i64) -> Document {
        let mut attributes = BTreeMap::new();
        attributes.insert("text".into(), AttributeValue::String(text.into()));
        attributes.insert("stars".into(), AttributeValue::Int(stars));
        Document {
            id: id.into(),
            vector: None,
            sparse_vector: None,
            attributes,
        }
    }

    #[test]
    fn build_index_and_roundtrip() {
        let seg = SegmentData::build(
            "s1".into(),
            vec![doc("a", "the quick brown fox", 3), doc("b", "lazy brown dog", 9)],
            vec!["gone".into()],
            1,
            2,
        );
        let fi = seg.text_index.get("text").unwrap();
        assert_eq!(fi.docs_with_field, 2);
        assert_eq!(fi.postings.get("brown").unwrap().len(), 2);
        assert_eq!(fi.doc_len[0], 4);

        let bytes = seg.encode().unwrap();
        let back = SegmentData::decode(&bytes).unwrap();
        assert_eq!(back.docs.len(), 2);
        assert_eq!(back.tombstones, vec!["gone".to_string()]);

        let mut corrupted = bytes.to_vec();
        let n = corrupted.len();
        corrupted[n / 2] ^= 0x1;
        assert!(SegmentData::decode(&corrupted).is_err());
    }

    #[test]
    fn range_pruning() {
        let seg = SegmentData::build(
            "s2".into(),
            vec![doc("a", "x", 3), doc("b", "y", 9)],
            vec![],
            1,
            2,
        );
        let cant = Filter::Gt {
            field: "stars".into(),
            value: AttributeValue::Int(100),
        };
        assert!(!seg.can_match(&cant));
        let can = Filter::Gte {
            field: "stars".into(),
            value: AttributeValue::Int(9),
        };
        assert!(seg.can_match(&can));
        // unknown field is conservative
        let unknown = Filter::Eq {
            field: "nope".into(),
            value: AttributeValue::Int(1),
        };
        assert!(seg.can_match(&unknown));
    }
}
