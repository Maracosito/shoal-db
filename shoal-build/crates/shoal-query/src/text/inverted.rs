//! Persistent inverted index segments.
//!
//! An [`InvertedSegment`] is an immutable, self-contained full-text index over
//! one storage segment's documents (addressed by their local `u32` doc
//! ordinals, the same ordinal space used by the filter doc-sets). Segments are
//! serialized to a single blob that lives in object storage under the
//! namespace's index prefix and is cached on local disk; on load, the term
//! dictionary and per-field statistics are materialized in memory while
//! posting lists stay as compact bytes and are decoded lazily per query term.
//!
//! ## On-disk layout (version 1)
//!
//! ```text
//! magic        "SHTX" (4 bytes)
//! version      u32 LE
//! num_fields   u16 LE
//! per field:   name_len u16 LE, name (utf8), doc_count u32 LE, total_len u64 LE
//! num_docs     u32 LE                       -- size of the doc-ordinal space
//! doc lengths: num_fields * num_docs uvarints (field-major)
//! num_terms    u32 LE
//! per term (sorted by term bytes):
//!     term_len u16 LE, term (utf8), entry_count u16 LE
//!     per entry: field u16 LE, doc_freq u32 LE, offset u32 LE, len u32 LE
//! blob_len     u32 LE
//! blob         postings bytes; per posting: uvarint(doc delta), uvarint(tf)
//! ```
//!
//! Doc ids inside a posting list are delta-encoded in ascending order; the
//! first delta is the absolute doc id.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

use crate::wire::{self, Reader, WireError};

use super::tokenizer::Tokenizer;

/// Magic bytes identifying an inverted text segment.
pub const SEGMENT_MAGIC: [u8; 4] = *b"SHTX";
/// Current segment format version.
pub const SEGMENT_VERSION: u32 = 1;

/// Errors produced by the full-text engine.
#[derive(Debug)]
pub enum TextError {
    /// Low-level decode failure (truncation, varint overflow).
    Wire(WireError),
    /// The blob does not start with the segment magic.
    BadMagic,
    /// The blob has an unknown format version.
    UnsupportedVersion(u32),
    /// A field name was not registered with the builder/segment.
    UnknownField(String),
    /// The same field name was registered twice.
    DuplicateField(String),
    /// A non-UTF8 string was found while decoding.
    InvalidUtf8,
    /// Structural corruption with a human-readable description.
    Corrupt(String),
}

impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextError::Wire(e) => write!(f, "wire decode error: {e}"),
            TextError::BadMagic => write!(f, "not an inverted text segment (bad magic)"),
            TextError::UnsupportedVersion(v) => {
                write!(f, "unsupported inverted segment version {v}")
            }
            TextError::UnknownField(name) => write!(f, "unknown text field: {name}"),
            TextError::DuplicateField(name) => write!(f, "duplicate text field: {name}"),
            TextError::InvalidUtf8 => write!(f, "invalid utf8 in segment"),
            TextError::Corrupt(msg) => write!(f, "corrupt inverted segment: {msg}"),
        }
    }
}

impl std::error::Error for TextError {}

impl From<WireError> for TextError {
    fn from(e: WireError) -> Self {
        TextError::Wire(e)
    }
}

/// Per-field statistics stored in the segment header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    /// Field name.
    pub name: String,
    /// Number of documents that contain at least one token in this field.
    pub doc_count: u32,
    /// Total number of tokens indexed in this field across all documents.
    pub total_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TermEntry {
    field: u16,
    doc_freq: u32,
    offset: u32,
    len: u32,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builds an [`InvertedSegment`] from documents.
///
/// Documents are addressed by their local `u32` ordinal. Calling
/// [`InvertedIndexBuilder::add_document`] twice with the same ordinal appends
/// the new text (term frequencies and field lengths accumulate).
#[derive(Debug)]
pub struct InvertedIndexBuilder {
    tokenizer: Tokenizer,
    field_names: Vec<String>,
    /// term -> field -> doc -> tf
    postings: BTreeMap<String, BTreeMap<u16, HashMap<u32, u32>>>,
    /// per field: doc -> token count
    doc_lens: Vec<HashMap<u32, u32>>,
    field_doc_counts: Vec<u32>,
    field_total_lens: Vec<u64>,
    max_doc: Option<u32>,
}

impl InvertedIndexBuilder {
    /// Create a builder over the given field names. Field ids are assigned in
    /// the order given (`fields[i]` gets id `i as u16`).
    pub fn new(tokenizer: Tokenizer, fields: &[&str]) -> Result<Self, TextError> {
        let mut seen = HashMap::new();
        for (i, name) in fields.iter().enumerate() {
            if seen.insert(name.to_string(), i).is_some() {
                return Err(TextError::DuplicateField(name.to_string()));
            }
        }
        let n = fields.len();
        Ok(InvertedIndexBuilder {
            tokenizer,
            field_names: fields.iter().map(|s| s.to_string()).collect(),
            postings: BTreeMap::new(),
            doc_lens: vec![HashMap::new(); n],
            field_doc_counts: vec![0; n],
            field_total_lens: vec![0; n],
            max_doc: None,
        })
    }

    /// Number of documents (distinct ordinals) seen so far is not tracked;
    /// this returns the ordinal-space size implied by the maximum doc id.
    pub fn num_docs(&self) -> u32 {
        self.max_doc.map(|d| d + 1).unwrap_or(0)
    }

    /// Index `fields` (name, text) for document ordinal `doc`.
    pub fn add_document(&mut self, doc: u32, fields: &[(&str, &str)]) -> Result<(), TextError> {
        self.max_doc = Some(self.max_doc.map_or(doc, |m| m.max(doc)));
        for (field_name, text) in fields {
            let fid = self
                .field_names
                .iter()
                .position(|n| n == field_name)
                .ok_or_else(|| TextError::UnknownField(field_name.to_string()))?;
            let tokens = self.tokenizer.tokenize(text);
            let len = tokens.len() as u32;
            if len == 0 {
                continue;
            }
            match self.doc_lens[fid].entry(doc) {
                Entry::Occupied(mut e) => *e.get_mut() += len,
                Entry::Vacant(e) => {
                    e.insert(len);
                    self.field_doc_counts[fid] += 1;
                }
            }
            self.field_total_lens[fid] += len as u64;
            let fid = fid as u16;
            for tok in tokens {
                *self
                    .postings
                    .entry(tok.text)
                    .or_default()
                    .entry(fid)
                    .or_default()
                    .entry(doc)
                    .or_insert(0) += 1;
            }
        }
        Ok(())
    }

    /// Finish building and produce an immutable segment.
    pub fn build(self) -> InvertedSegment {
        let num_docs = self.num_docs();
        let num_fields = self.field_names.len();

        let mut doc_lens = vec![vec![0u32; num_docs as usize]; num_fields];
        for (f, lens) in self.doc_lens.iter().enumerate() {
            for (&doc, &len) in lens {
                doc_lens[f][doc as usize] = len;
            }
        }

        let mut blob: Vec<u8> = Vec::new();
        let mut terms: BTreeMap<String, Vec<TermEntry>> = BTreeMap::new();
        for (term, by_field) in self.postings {
            let mut entries = Vec::with_capacity(by_field.len());
            for (&field, docs) in &by_field {
                let mut posting: Vec<(u32, u32)> =
                    docs.iter().map(|(&d, &tf)| (d, tf)).collect();
                posting.sort_unstable_by_key(|p| p.0);
                let offset = blob.len() as u32;
                let mut prev = 0u32;
                let mut first = true;
                for (doc, tf) in &posting {
                    let delta = if first { *doc } else { *doc - prev };
                    wire::put_uvarint(&mut blob, delta as u64);
                    wire::put_uvarint(&mut blob, *tf as u64);
                    prev = *doc;
                    first = false;
                }
                entries.push(TermEntry {
                    field,
                    doc_freq: posting.len() as u32,
                    offset,
                    len: blob.len() as u32 - offset,
                });
            }
            terms.insert(term, entries);
        }

        let fields = self
            .field_names
            .into_iter()
            .enumerate()
            .map(|(i, name)| FieldInfo {
                name,
                doc_count: self.field_doc_counts[i],
                total_len: self.field_total_lens[i],
            })
            .collect();

        InvertedSegment {
            fields,
            num_docs,
            doc_lens,
            terms,
            postings_blob: blob,
        }
    }
}

// ---------------------------------------------------------------------------
// Segment
// ---------------------------------------------------------------------------

/// An immutable full-text index over one segment's documents.
#[derive(Debug)]
pub struct InvertedSegment {
    fields: Vec<FieldInfo>,
    num_docs: u32,
    /// Per field, dense doc-length array of size `num_docs`.
    doc_lens: Vec<Vec<u32>>,
    terms: BTreeMap<String, Vec<TermEntry>>,
    postings_blob: Vec<u8>,
}

impl InvertedSegment {
    /// Number of fields in this segment.
    pub fn num_fields(&self) -> usize {
        self.fields.len()
    }

    /// Size of the doc-ordinal space covered by this segment.
    pub fn num_docs(&self) -> u32 {
        self.num_docs
    }

    /// Number of distinct terms in the dictionary.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Iterate over all dictionary terms in sorted order.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.terms.keys().map(|s| s.as_str())
    }

    /// Resolve a field name to its id.
    pub fn field_id(&self, name: &str) -> Option<u16> {
        self.fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| i as u16)
    }

    /// Statistics for a field.
    pub fn field_stats(&self, field: u16) -> Option<&FieldInfo> {
        self.fields.get(field as usize)
    }

    /// Token length of `doc` in `field` (0 if absent or out of range).
    pub fn doc_len(&self, field: u16, doc: u32) -> u32 {
        self.doc_lens
            .get(field as usize)
            .and_then(|v| v.get(doc as usize))
            .copied()
            .unwrap_or(0)
    }

    /// Average token length of `field` over documents that contain it.
    pub fn avg_field_len(&self, field: u16) -> f32 {
        match self.field_stats(field) {
            Some(s) if s.doc_count > 0 => s.total_len as f32 / s.doc_count as f32,
            _ => 0.0,
        }
    }

    /// Number of documents containing `term` in `field`.
    pub fn doc_freq(&self, term: &str, field: u16) -> u32 {
        self.terms
            .get(term)
            .and_then(|es| es.iter().find(|e| e.field == field))
            .map(|e| e.doc_freq)
            .unwrap_or(0)
    }

    /// Lazily decode the posting list for `(term, field)`.
    pub fn postings(&self, term: &str, field: u16) -> Option<PostingsCursor<'_>> {
        let entry = self
            .terms
            .get(term)?
            .iter()
            .find(|e| e.field == field)?;
        let start = entry.offset as usize;
        let end = start + entry.len as usize;
        let slice = &self.postings_blob[start..end];
        Some(PostingsCursor {
            reader: Reader::new(slice),
            remaining: entry.doc_freq,
            prev: 0,
            first: true,
        })
    }

    /// Serialize the segment into a single blob.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SEGMENT_MAGIC);
        wire::put_u32(&mut out, SEGMENT_VERSION);
        wire::put_u16(&mut out, self.fields.len() as u16);
        for f in &self.fields {
            wire::put_u16(&mut out, f.name.len() as u16);
            out.extend_from_slice(f.name.as_bytes());
            wire::put_u32(&mut out, f.doc_count);
            wire::put_u64(&mut out, f.total_len);
        }
        wire::put_u32(&mut out, self.num_docs);
        for field_lens in &self.doc_lens {
            for &len in field_lens {
                wire::put_uvarint(&mut out, len as u64);
            }
        }
        wire::put_u32(&mut out, self.terms.len() as u32);
        for (term, entries) in &self.terms {
            wire::put_u16(&mut out, term.len() as u16);
            out.extend_from_slice(term.as_bytes());
            wire::put_u16(&mut out, entries.len() as u16);
            for e in entries {
                wire::put_u16(&mut out, e.field);
                wire::put_u32(&mut out, e.doc_freq);
                wire::put_u32(&mut out, e.offset);
                wire::put_u32(&mut out, e.len);
            }
        }
        wire::put_u32(&mut out, self.postings_blob.len() as u32);
        out.extend_from_slice(&self.postings_blob);
        out
    }

    /// Deserialize a segment, validating structure and bounds. Returns an
    /// error (never panics) on truncated or corrupt input.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TextError> {
        let mut r = Reader::new(bytes);
        let magic = r.take(4)?;
        if magic != SEGMENT_MAGIC {
            return Err(TextError::BadMagic);
        }
        let version = r.read_u32()?;
        if version != SEGMENT_VERSION {
            return Err(TextError::UnsupportedVersion(version));
        }
        let num_fields = r.read_u16()? as usize;
        let mut fields = Vec::new();
        for _ in 0..num_fields {
            let name_len = r.read_u16()? as usize;
            let name_bytes = r.take(name_len)?;
            let name =
                String::from_utf8(name_bytes.to_vec()).map_err(|_| TextError::InvalidUtf8)?;
            let doc_count = r.read_u32()?;
            let total_len = r.read_u64()?;
            fields.push(FieldInfo {
                name,
                doc_count,
                total_len,
            });
        }
        let num_docs = r.read_u32()?;
        // Each doc length is at least one byte; reject impossible sizes
        // before allocating.
        let needed = num_docs as u64 * num_fields as u64;
        if needed > r.remaining() as u64 {
            return Err(TextError::Corrupt(format!(
                "doc-length table needs at least {needed} bytes, {} remain",
                r.remaining()
            )));
        }
        let mut doc_lens = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            let mut lens = Vec::with_capacity(num_docs as usize);
            for _ in 0..num_docs {
                lens.push(r.read_uvarint_u32()?);
            }
            doc_lens.push(lens);
        }
        let num_terms = r.read_u32()?;
        let mut terms = BTreeMap::new();
        for _ in 0..num_terms {
            let term_len = r.read_u16()? as usize;
            let term_bytes = r.take(term_len)?;
            let term =
                String::from_utf8(term_bytes.to_vec()).map_err(|_| TextError::InvalidUtf8)?;
            let entry_count = r.read_u16()? as usize;
            let mut entries = Vec::new();
            for _ in 0..entry_count {
                let field = r.read_u16()?;
                if field as usize >= num_fields {
                    return Err(TextError::Corrupt(format!(
                        "term '{term}' references field {field}, only {num_fields} declared"
                    )));
                }
                let doc_freq = r.read_u32()?;
                let offset = r.read_u32()?;
                let len = r.read_u32()?;
                entries.push(TermEntry {
                    field,
                    doc_freq,
                    offset,
                    len,
                });
            }
            terms.insert(term, entries);
        }
        let blob_len = r.read_u32()? as usize;
        let blob = r.take(blob_len)?.to_vec();
        // Validate posting ranges against the blob.
        for (term, entries) in &terms {
            for e in entries {
                let end = e.offset as u64 + e.len as u64;
                if end > blob.len() as u64 {
                    return Err(TextError::Corrupt(format!(
                        "posting range for term '{term}' out of bounds"
                    )));
                }
            }
        }
        Ok(InvertedSegment {
            fields,
            num_docs,
            doc_lens,
            terms,
            postings_blob: blob,
        })
    }
}

/// Lazily decodes a posting list. Yields `(doc, term_frequency)` pairs in
/// ascending doc order.
#[derive(Debug)]
pub struct PostingsCursor<'a> {
    reader: Reader<'a>,
    remaining: u32,
    prev: u32,
    first: bool,
}

impl Iterator for PostingsCursor<'_> {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        if self.remaining == 0 {
            return None;
        }
        // Posting ranges are bounds-validated at load time, so decode errors
        // here indicate corruption inside a valid range; we stop iteration
        // rather than panic.
        let delta = self.reader.read_uvarint_u32().ok()?;
        let tf = self.reader.read_uvarint_u32().ok()?;
        let doc = if self.first { delta } else { self.prev + delta };
        self.first = false;
        self.prev = doc;
        self.remaining -= 1;
        Some((doc, tf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment() -> InvertedSegment {
        let mut b = InvertedIndexBuilder::new(Tokenizer::default(), &["title", "body"]).unwrap();
        b.add_document(0, &[("title", "Rust guide"), ("body", "rust database storage")])
            .unwrap();
        b.add_document(2, &[("body", "database engine engine")])
            .unwrap();
        b.build()
    }

    #[test]
    fn builder_stats() {
        let seg = segment();
        assert_eq!(seg.num_docs(), 3); // ordinal space covers doc 0..=2
        assert_eq!(seg.num_fields(), 2);
        let title = seg.field_stats(seg.field_id("title").unwrap()).unwrap();
        assert_eq!(title.doc_count, 1);
        assert_eq!(title.total_len, 2);
        let body = seg.field_stats(seg.field_id("body").unwrap()).unwrap();
        assert_eq!(body.doc_count, 2);
        assert_eq!(body.total_len, 6);
        assert_eq!(seg.doc_len(1, 0), 3);
        assert_eq!(seg.doc_len(1, 1), 0); // doc 1 never indexed
        assert_eq!(seg.doc_len(1, 2), 3);
    }

    #[test]
    fn postings_decode() {
        let seg = segment();
        let body = seg.field_id("body").unwrap();
        let postings: Vec<_> = seg.postings("database", body).unwrap().collect();
        assert_eq!(postings, vec![(0, 1), (2, 1)]);
        let postings: Vec<_> = seg.postings("engine", body).unwrap().collect();
        assert_eq!(postings, vec![(2, 2)]);
        assert!(seg.postings("missing", body).is_none());
        assert_eq!(seg.doc_freq("database", body), 2);
        assert_eq!(seg.doc_freq("database", seg.field_id("title").unwrap()), 0);
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let seg = segment();
        let bytes = seg.to_bytes();
        let seg2 = InvertedSegment::from_bytes(&bytes).unwrap();
        assert_eq!(seg2.num_docs(), seg.num_docs());
        assert_eq!(seg2.term_count(), seg.term_count());
        for term in seg.terms() {
            for field in 0..seg.num_fields() as u16 {
                assert_eq!(seg.doc_freq(term, field), seg2.doc_freq(term, field));
                let a: Option<Vec<_>> = seg.postings(term, field).map(|c| c.collect());
                let b: Option<Vec<_>> = seg2.postings(term, field).map(|c| c.collect());
                assert_eq!(a, b);
            }
        }
        for field in 0..seg.num_fields() as u16 {
            for doc in 0..seg.num_docs() {
                assert_eq!(seg.doc_len(field, doc), seg2.doc_len(field, doc));
            }
            assert_eq!(seg.field_stats(field), seg2.field_stats(field));
        }
    }

    #[test]
    fn duplicate_field_rejected() {
        let err = InvertedIndexBuilder::new(Tokenizer::default(), &["a", "a"]).unwrap_err();
        assert!(matches!(err, TextError::DuplicateField(_)));
    }

    #[test]
    fn unknown_field_rejected() {
        let mut b = InvertedIndexBuilder::new(Tokenizer::default(), &["body"]).unwrap();
        let err = b.add_document(0, &[("nope", "text")]).unwrap_err();
        assert!(matches!(err, TextError::UnknownField(_)));
    }

    #[test]
    fn empty_segment() {
        let b = InvertedIndexBuilder::new(Tokenizer::default(), &["body"]).unwrap();
        let seg = b.build();
        assert_eq!(seg.num_docs(), 0);
        assert_eq!(seg.term_count(), 0);
        let seg2 = InvertedSegment::from_bytes(&seg.to_bytes()).unwrap();
        assert_eq!(seg2.num_docs(), 0);
    }
}
