//! Immutable, columnar, checksummed segment files.
//!
//! Segments are the queryable unit produced by folding WAL entries (and by
//! compaction merges). They are written once to object storage and never
//! mutated. A segment stores rows sorted by document id, with each logical
//! column laid out as a contiguous, independently-checksummed block so that
//! readers can fetch/verify only the columns a query needs.
//!
//! On-disk layout:
//!
//! ```text
//! +--------------------------------------------------+
//! | header: magic "REEFSEG1" (8) | version u32 | rsvd |
//! +--------------------------------------------------+
//! | block 0 payload                                   |
//! | block 1 payload                                   |
//! | ...                                               |
//! +--------------------------------------------------+
//! | footer JSON (block index, counts, wal range, ...) |
//! | footer crc32c (u32 LE)                            |
//! | footer length  (u32 LE)                           |
//! | magic "REEFSEG1" (8)                              |
//! +--------------------------------------------------+
//! ```
//!
//! Standard blocks:
//! * `ids`        — JSON array of document ids, ascending (binary-searchable).
//! * `tombstones` — JSON array of row indices that are delete tombstones.
//! * `vec.rows`   — JSON array of row indices that carry a dense vector.
//! * `vec.data`   — packed little-endian f32, `vec.rows.len() * dim` values.
//! * `sparse`     — JSON array (len = rows) of optional sparse vectors.
//! * `attr:<name>`— JSON array (len = rows) of optional attribute values.
//!
//! Every block records its own CRC32C in the footer; readers verify a block
//! the first time it is decoded. Truncated files, bit flips, and missing
//! trailers all surface as [`EngineError::Corruption`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use reef_types::{DocId, Document, SparseVector, Value};

use crate::checksum::crc32c;
use crate::error::{EngineError, EngineResult};
use crate::manifest::SegmentRef;

pub const SEGMENT_MAGIC: &[u8; 8] = b"REEFSEG1";
pub const SEGMENT_FORMAT_VERSION: u32 = 1;

const HEADER_LEN: usize = 16; // magic(8) + version(4) + reserved(4)
const TRAILER_LEN: usize = 16; // crc(4) + len(4) + magic(8)

pub const BLOCK_IDS: &str = "ids";
pub const BLOCK_TOMBSTONES: &str = "tombstones";
pub const BLOCK_VEC_ROWS: &str = "vec.rows";
pub const BLOCK_VEC_DATA: &str = "vec.data";
pub const BLOCK_SPARSE: &str = "sparse";
pub const ATTR_BLOCK_PREFIX: &str = "attr:";

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Encoding of a block payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// serde_json-encoded payload.
    Json,
    /// Raw packed little-endian f32 values.
    RawF32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockMeta {
    pub name: String,
    pub kind: BlockKind,
    pub offset: u64,
    pub length: u64,
    pub crc32c: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentFooter {
    pub format_version: u32,
    pub segment_id: String,
    pub namespace: String,
    pub row_count: u64,
    pub live_count: u64,
    pub tombstone_count: u64,
    /// Dense vector dimensionality, if any row carries a vector.
    pub vector_dim: Option<u32>,
    pub min_wal_seq: u64,
    pub max_wal_seq: u64,
    pub created_at_ms: u64,
    pub blocks: Vec<BlockMeta>,
}

impl SegmentFooter {
    /// Build the manifest entry for this segment.
    pub fn to_segment_ref(&self, size_bytes: u64, level: u32, owner_namespace: &str) -> SegmentRef {
        SegmentRef {
            segment_id: self.segment_id.clone(),
            level,
            doc_count: self.row_count,
            live_doc_count: self.live_count,
            tombstone_count: self.tombstone_count,
            size_bytes,
            min_wal_seq: self.min_wal_seq,
            max_wal_seq: self.max_wal_seq,
            created_at_ms: self.created_at_ms,
            owner_namespace: owner_namespace.to_string(),
        }
    }
}

/// One logical row of a segment: a live document or a delete tombstone.
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentRow {
    Live(Document),
    Tombstone(DocId),
}

impl SegmentRow {
    pub fn id(&self) -> &DocId {
        match self {
            SegmentRow::Live(doc) => &doc.id,
            SegmentRow::Tombstone(id) => id,
        }
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(self, SegmentRow::Tombstone(_))
    }
}

/// Fully encoded segment ready to be written to object storage.
pub struct EncodedSegment {
    pub bytes: Bytes,
    pub footer: SegmentFooter,
}

impl EncodedSegment {
    pub fn size_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Streaming builder. Rows must be pushed in strictly ascending id order
/// (the natural order of a memtable drain or a merge cursor).
pub struct SegmentBuilder {
    namespace: String,
    segment_id: String,
    min_wal_seq: u64,
    max_wal_seq: u64,
    rows: Vec<SegmentRow>,
}

impl SegmentBuilder {
    pub fn new(
        namespace: impl Into<String>,
        segment_id: impl Into<String>,
        min_wal_seq: u64,
        max_wal_seq: u64,
    ) -> Self {
        SegmentBuilder {
            namespace: namespace.into(),
            segment_id: segment_id.into(),
            min_wal_seq,
            max_wal_seq,
            rows: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn push(&mut self, row: SegmentRow) -> EngineResult<()> {
        if let Some(last) = self.rows.last() {
            if row.id() <= last.id() {
                return Err(EngineError::InvalidArgument(format!(
                    "segment rows must be strictly ascending by id: {:?} after {:?}",
                    row.id(),
                    last.id()
                )));
            }
        }
        self.rows.push(row);
        Ok(())
    }

    pub fn push_live(&mut self, doc: Document) -> EngineResult<()> {
        self.push(SegmentRow::Live(doc))
    }

    pub fn push_tombstone(&mut self, id: DocId) -> EngineResult<()> {
        self.push(SegmentRow::Tombstone(id))
    }

    pub fn finish(self) -> EngineResult<EncodedSegment> {
        if self.rows.is_empty() {
            return Err(EngineError::InvalidArgument(
                "cannot encode an empty segment".into(),
            ));
        }

        let row_count = self.rows.len();
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        buf.extend_from_slice(SEGMENT_MAGIC);
        buf.extend_from_slice(&SEGMENT_FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        debug_assert_eq!(buf.len(), HEADER_LEN);

        let mut blocks: Vec<BlockMeta> = Vec::new();

        // --- ids ------------------------------------------------------
        let ids: Vec<&DocId> = self.rows.iter().map(|r| r.id()).collect();
        append_json_block(&mut buf, &mut blocks, BLOCK_IDS, &ids)?;

        // --- tombstones -----------------------------------------------
        let tombstone_rows: Vec<u32> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_tombstone())
            .map(|(i, _)| i as u32)
            .collect();
        let tombstone_count = tombstone_rows.len() as u64;
        if !tombstone_rows.is_empty() {
            append_json_block(&mut buf, &mut blocks, BLOCK_TOMBSTONES, &tombstone_rows)?;
        }

        // --- dense vectors --------------------------------------------
        let mut vec_rows: Vec<u32> = Vec::new();
        let mut vec_dim: Option<usize> = None;
        let mut vec_data: Vec<u8> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            if let SegmentRow::Live(doc) = row {
                if let Some(v) = &doc.vector {
                    match vec_dim {
                        None => vec_dim = Some(v.len()),
                        Some(d) if d != v.len() => {
                            return Err(EngineError::InvalidArgument(format!(
                                "inconsistent vector dimensions in segment: {} vs {}",
                                d,
                                v.len()
                            )));
                        }
                        _ => {}
                    }
                    vec_rows.push(i as u32);
                    for f in v {
                        vec_data.extend_from_slice(&f.to_le_bytes());
                    }
                }
            }
        }
        if !vec_rows.is_empty() {
            append_json_block(&mut buf, &mut blocks, BLOCK_VEC_ROWS, &vec_rows)?;
            append_raw_block(&mut buf, &mut blocks, BLOCK_VEC_DATA, vec_data);
        }

        // --- sparse vectors -------------------------------------------
        let sparse: Vec<Option<&SparseVector>> = self
            .rows
            .iter()
            .map(|r| match r {
                SegmentRow::Live(doc) => doc.sparse_vector.as_ref(),
                SegmentRow::Tombstone(_) => None,
            })
            .collect();
        if sparse.iter().any(|s| s.is_some()) {
            append_json_block(&mut buf, &mut blocks, BLOCK_SPARSE, &sparse)?;
        }

        // --- attribute columns ----------------------------------------
        let mut attr_names: BTreeSet<&str> = BTreeSet::new();
        for row in &self.rows {
            if let SegmentRow::Live(doc) = row {
                for name in doc.attributes.keys() {
                    attr_names.insert(name.as_str());
                }
            }
        }
        for name in attr_names {
            let col: Vec<Option<&Value>> = self
                .rows
                .iter()
                .map(|r| match r {
                    SegmentRow::Live(doc) => doc.attributes.get(name),
                    SegmentRow::Tombstone(_) => None,
                })
                .collect();
            let block_name = format!("{ATTR_BLOCK_PREFIX}{name}");
            append_json_block(&mut buf, &mut blocks, &block_name, &col)?;
        }

        // --- footer + trailer ------------------------------------------
        let footer = SegmentFooter {
            format_version: SEGMENT_FORMAT_VERSION,
            segment_id: self.segment_id,
            namespace: self.namespace,
            row_count: row_count as u64,
            live_count: row_count as u64 - tombstone_count,
            tombstone_count,
            vector_dim: vec_dim.map(|d| d as u32),
            min_wal_seq: self.min_wal_seq,
            max_wal_seq: self.max_wal_seq,
            created_at_ms: unix_ms(),
            blocks,
        };
        let footer_json = serde_json::to_vec(&footer)
            .map_err(|e| EngineError::Corruption(format!("footer serialization failed: {e}")))?;
        let footer_crc = crc32c(&footer_json);
        let footer_len = footer_json.len() as u32;
        buf.extend_from_slice(&footer_json);
        buf.extend_from_slice(&footer_crc.to_le_bytes());
        buf.extend_from_slice(&footer_len.to_le_bytes());
        buf.extend_from_slice(SEGMENT_MAGIC);

        Ok(EncodedSegment {
            bytes: Bytes::from(buf),
            footer,
        })
    }
}

fn append_json_block<T: Serialize>(
    buf: &mut Vec<u8>,
    blocks: &mut Vec<BlockMeta>,
    name: &str,
    value: &T,
) -> EngineResult<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|e| EngineError::Corruption(format!("block {name} serialization failed: {e}")))?;
    append_block(buf, blocks, name, BlockKind::Json, payload);
    Ok(())
}

fn append_raw_block(buf: &mut Vec<u8>, blocks: &mut Vec<BlockMeta>, name: &str, payload: Vec<u8>) {
    append_block(buf, blocks, name, BlockKind::RawF32, payload);
}

fn append_block(
    buf: &mut Vec<u8>,
    blocks: &mut Vec<BlockMeta>,
    name: &str,
    kind: BlockKind,
    payload: Vec<u8>,
) {
    let meta = BlockMeta {
        name: name.to_string(),
        kind,
        offset: buf.len() as u64,
        length: payload.len() as u64,
        crc32c: crc32c(&payload),
    };
    buf.extend_from_slice(&payload);
    blocks.push(meta);
}

/// Thread-safe lazily decoded, checksum-verified column.
struct Lazy<T> {
    cell: RwLock<Option<Arc<T>>>,
}

impl<T> Lazy<T> {
    fn new() -> Self {
        Lazy {
            cell: RwLock::new(None),
        }
    }

    fn get_or_try<F: FnOnce() -> EngineResult<T>>(&self, f: F) -> EngineResult<Arc<T>> {
        if let Some(v) = self.cell.read().expect("lazy lock poisoned").as_ref() {
            return Ok(v.clone());
        }
        let mut w = self.cell.write().expect("lazy lock poisoned");
        if let Some(v) = w.as_ref() {
            return Ok(v.clone());
        }
        let v = Arc::new(f()?);
        *w = Some(v.clone());
        Ok(v)
    }
}

/// Reader over a fully fetched segment object. Columns are decoded lazily,
/// verified against their CRC on first access, and cached.
pub struct SegmentReader {
    data: Bytes,
    footer: SegmentFooter,
    ids: Lazy<Vec<DocId>>,
    tombstones: Lazy<Vec<u32>>,
    vec_rows: Lazy<Vec<u32>>,
    vec_data: Lazy<Vec<f32>>,
    sparse: Lazy<Vec<Option<SparseVector>>>,
    attrs: RwLock<HashMap<String, Arc<Vec<Option<Value>>>>>,
}

impl SegmentReader {
    /// Parse and verify the trailer/footer; payload blocks are verified lazily.
    pub fn open(data: Bytes) -> EngineResult<Self> {
        if data.len() < HEADER_LEN + TRAILER_LEN {
            return Err(EngineError::Corruption(format!(
                "segment too small: {} bytes",
                data.len()
            )));
        }
        if &data[..8] != SEGMENT_MAGIC {
            return Err(EngineError::Corruption("segment header magic mismatch".into()));
        }
        let version = u32_le(&data[8..12]);
        if version != SEGMENT_FORMAT_VERSION {
            return Err(EngineError::Corruption(format!(
                "unsupported segment format version {version}"
            )));
        }
        let end = data.len();
        if &data[end - 8..] != SEGMENT_MAGIC {
            return Err(EngineError::Corruption("segment trailer magic mismatch".into()));
        }
        let footer_len = u32_le(&data[end - 12..end - 8]) as usize;
        let footer_crc = u32_le(&data[end - 16..end - 12]);
        let footer_end = end - TRAILER_LEN;
        if footer_len > footer_end || footer_end - footer_len < HEADER_LEN {
            return Err(EngineError::Corruption(format!(
                "segment footer length {footer_len} out of bounds"
            )));
        }
        let footer_start = footer_end - footer_len;
        let footer_bytes = &data[footer_start..footer_end];
        let actual = crc32c(footer_bytes);
        if actual != footer_crc {
            return Err(EngineError::Corruption(format!(
                "segment footer checksum mismatch: expected {footer_crc:#010x}, computed {actual:#010x}"
            )));
        }
        let footer: SegmentFooter = serde_json::from_slice(footer_bytes)
            .map_err(|e| EngineError::Corruption(format!("segment footer unreadable: {e}")))?;
        if footer.live_count + footer.tombstone_count != footer.row_count {
            return Err(EngineError::Corruption(
                "segment footer row accounting mismatch".into(),
            ));
        }
        for b in &footer.blocks {
            let bo = b.offset as usize;
            let bl = b.length as usize;
            if bo < HEADER_LEN || bo.checked_add(bl).map_or(true, |e| e > footer_start) {
                return Err(EngineError::Corruption(format!(
                    "segment block {:?} extends outside data region",
                    b.name
                )));
            }
        }
        Ok(SegmentReader {
            data,
            footer,
            ids: Lazy::new(),
            tombstones: Lazy::new(),
            vec_rows: Lazy::new(),
            vec_data: Lazy::new(),
            sparse: Lazy::new(),
            attrs: RwLock::new(HashMap::new()),
        })
    }

    pub fn footer(&self) -> &SegmentFooter {
        &self.footer
    }

    pub fn row_count(&self) -> usize {
        self.footer.row_count as usize
    }

    fn block_meta(&self, name: &str) -> Option<&BlockMeta> {
        self.footer.blocks.iter().find(|b| b.name == name)
    }

    /// Fetch and CRC-verify a block's raw bytes.
    fn block_bytes(&self, meta: &BlockMeta) -> EngineResult<&[u8]> {
        let start = meta.offset as usize;
        let end = start + meta.length as usize;
        let bytes = &self.data[start..end];
        let actual = crc32c(bytes);
        if actual != meta.crc32c {
            return Err(EngineError::Corruption(format!(
                "segment {} block {:?} checksum mismatch: expected {:#010x}, computed {actual:#010x}",
                self.footer.segment_id, meta.name, meta.crc32c
            )));
        }
        Ok(bytes)
    }

    fn json_block<T: for<'de> Deserialize<'de>>(&self, name: &str) -> EngineResult<Option<T>> {
        let Some(meta) = self.block_meta(name) else {
            return Ok(None);
        };
        let bytes = self.block_bytes(meta)?;
        let value = serde_json::from_slice(bytes).map_err(|e| {
            EngineError::Corruption(format!(
                "segment {} block {name:?} undecodable: {e}",
                self.footer.segment_id
            ))
        })?;
        Ok(Some(value))
    }

    /// Document ids, ascending.
    pub fn ids(&self) -> EngineResult<Arc<Vec<DocId>>> {
        self.ids.get_or_try(|| {
            let ids: Vec<DocId> = self
                .json_block(BLOCK_IDS)?
                .ok_or_else(|| EngineError::Corruption("segment missing ids block".into()))?;
            if ids.len() != self.footer.row_count as usize {
                return Err(EngineError::Corruption(format!(
                    "ids block has {} rows, footer says {}",
                    ids.len(),
                    self.footer.row_count
                )));
            }
            Ok(ids)
        })
    }

    fn tombstone_rows(&self) -> EngineResult<Arc<Vec<u32>>> {
        self.tombstones
            .get_or_try(|| Ok(self.json_block(BLOCK_TOMBSTONES)?.unwrap_or_default()))
    }

    /// Binary search a document id; `Ok(Some(row))` if present (live OR tombstone).
    pub fn find(&self, id: &DocId) -> EngineResult<Option<u32>> {
        let ids = self.ids()?;
        Ok(ids.binary_search(id).ok().map(|i| i as u32))
    }

    pub fn is_tombstone(&self, row: u32) -> EngineResult<bool> {
        Ok(self.tombstone_rows()?.binary_search(&row).is_ok())
    }

    /// Dense vector for a row, if present.
    pub fn vector(&self, row: u32) -> EngineResult<Option<Vec<f32>>> {
        if self.block_meta(BLOCK_VEC_ROWS).is_none() {
            return Ok(None);
        }
        let rows = self.vec_rows.get_or_try(|| {
            Ok(self.json_block(BLOCK_VEC_ROWS)?.unwrap_or_default())
        })?;
        let Ok(pos) = rows.binary_search(&row) else {
            return Ok(None);
        };
        let dim = self.footer.vector_dim.ok_or_else(|| {
            EngineError::Corruption("segment has vector blocks but no vector_dim".into())
        })? as usize;
        let data = self.vec_data.get_or_try(|| {
            let meta = self
                .block_meta(BLOCK_VEC_DATA)
                .ok_or_else(|| EngineError::Corruption("vec.rows present but vec.data missing".into()))?;
            let bytes = self.block_bytes(meta)?;
            if bytes.len() != rows.len() * dim * 4 {
                return Err(EngineError::Corruption(format!(
                    "vec.data length {} != rows {} * dim {dim} * 4",
                    bytes.len(),
                    rows.len()
                )));
            }
            let mut out = Vec::with_capacity(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            Ok(out)
        })?;
        let start = pos * dim;
        Ok(Some(data[start..start + dim].to_vec()))
    }

    fn sparse_col(&self) -> EngineResult<Arc<Vec<Option<SparseVector>>>> {
        self.sparse.get_or_try(|| {
            Ok(self
                .json_block(BLOCK_SPARSE)?
                .unwrap_or_else(|| vec![None; self.footer.row_count as usize]))
        })
    }

    fn attr_col(&self, name: &str) -> EngineResult<Arc<Vec<Option<Value>>>> {
        if let Some(col) = self.attrs.read().expect("attrs lock poisoned").get(name) {
            return Ok(col.clone());
        }
        let block_name = format!("{ATTR_BLOCK_PREFIX}{name}");
        let col: Vec<Option<Value>> = self
            .json_block(&block_name)?
            .unwrap_or_else(|| vec![None; self.footer.row_count as usize]);
        let col = Arc::new(col);
        self.attrs
            .write()
            .expect("attrs lock poisoned")
            .insert(name.to_string(), col.clone());
        Ok(col)
    }

    /// Names of all attribute columns stored in this segment.
    pub fn attribute_names(&self) -> Vec<&str> {
        self.footer
            .blocks
            .iter()
            .filter_map(|b| b.name.strip_prefix(ATTR_BLOCK_PREFIX))
            .collect()
    }

    /// Reconstruct the logical row at an index.
    pub fn row(&self, row: u32) -> EngineResult<SegmentRow> {
        let ids = self.ids()?;
        let idx = row as usize;
        if idx >= ids.len() {
            return Err(EngineError::InvalidArgument(format!(
                "row {row} out of bounds (segment has {} rows)",
                ids.len()
            )));
        }
        let id = ids[idx].clone();
        if self.is_tombstone(row)? {
            return Ok(SegmentRow::Tombstone(id));
        }
        let vector = self.vector(row)?;
        let sparse_vector = if self.block_meta(BLOCK_SPARSE).is_some() {
            self.sparse_col()?[idx].clone()
        } else {
            None
        };
        let mut attributes = BTreeMap::new();
        let names: Vec<String> = self
            .attribute_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        for name in names {
            let col = self.attr_col(&name)?;
            if let Some(v) = col[idx].clone() {
                attributes.insert(name, v);
            }
        }
        Ok(SegmentRow::Live(Document {
            id,
            vector,
            sparse_vector,
            attributes,
        }))
    }

    /// Lookup a document by id. `Ok(Some(row))` may be a tombstone — callers
    /// merging segments must respect it (it shadows older segments).
    pub fn get(&self, id: &DocId) -> EngineResult<Option<SegmentRow>> {
        match self.find(id)? {
            Some(row) => Ok(Some(self.row(row)?)),
            None => Ok(None),
        }
    }

    /// Materialize every row (id order). Used by compaction merges and export.
    pub fn rows(&self) -> EngineResult<Vec<SegmentRow>> {
        let n = self.row_count();
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.row(i as u32)?);
        }
        Ok(out)
    }

    /// Eagerly decode and CRC-verify every block. Used by `reef verify`.
    pub fn verify_all_blocks(&self) -> EngineResult<()> {
        for meta in &self.footer.blocks {
            self.block_bytes(meta)?;
        }
        // Also exercise the structured decoders so JSON-level corruption
        // (valid CRC, bad content) is caught.
        self.ids()?;
        self.rows()?;
        Ok(())
    }
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, vector: Option<Vec<f32>>, tag: i64) -> Document {
        let mut attributes = BTreeMap::new();
        attributes.insert("tag".to_string(), Value::Int(tag));
        attributes.insert(
            "title".to_string(),
            Value::String(format!("title of {id}")),
        );
        Document {
            id: DocId::from(id),
            vector,
            sparse_vector: None,
            attributes,
        }
    }

    fn build_sample() -> EncodedSegment {
        let mut b = SegmentBuilder::new("ns-a", "seg-0001", 1, 9);
        b.push_live(doc("a", Some(vec![0.1, 0.2, 0.3]), 1)).unwrap();
        b.push_live(doc("b", None, 2)).unwrap();
        b.push_tombstone(DocId::from("c")).unwrap();
        let mut d = doc("d", Some(vec![1.0, -1.0, 0.5]), 4);
        d.sparse_vector = Some(SparseVector {
            indices: vec![3, 17, 42],
            values: vec![0.5, 1.5, -0.25],
        });
        b.push_live(d).unwrap();
        b.finish().unwrap()
    }

    #[test]
    fn roundtrip_preserves_rows() {
        let enc = build_sample();
        assert_eq!(enc.footer.row_count, 4);
        assert_eq!(enc.footer.live_count, 3);
        assert_eq!(enc.footer.tombstone_count, 1);
        assert_eq!(enc.footer.vector_dim, Some(3));

        let r = SegmentReader::open(enc.bytes.clone()).unwrap();
        assert_eq!(r.row_count(), 4);

        // Ids ascending and searchable.
        let ids = r.ids().unwrap();
        assert_eq!(ids.len(), 4);
        assert_eq!(r.find(&DocId::from("b")).unwrap(), Some(1));
        assert_eq!(r.find(&DocId::from("zzz")).unwrap(), None);

        // Live document fully reconstructed.
        match r.get(&DocId::from("a")).unwrap().unwrap() {
            SegmentRow::Live(d) => {
                assert_eq!(d.id, DocId::from("a"));
                assert_eq!(d.vector, Some(vec![0.1, 0.2, 0.3]));
                assert_eq!(d.attributes.get("tag"), Some(&Value::Int(1)));
                assert_eq!(
                    d.attributes.get("title"),
                    Some(&Value::String("title of a".into()))
                );
            }
            other => panic!("expected live row, got {other:?}"),
        }

        // Row without a vector.
        match r.get(&DocId::from("b")).unwrap().unwrap() {
            SegmentRow::Live(d) => assert_eq!(d.vector, None),
            other => panic!("expected live row, got {other:?}"),
        }

        // Tombstone is surfaced, not hidden.
        assert!(matches!(
            r.get(&DocId::from("c")).unwrap().unwrap(),
            SegmentRow::Tombstone(_)
        ));
        assert!(r.is_tombstone(2).unwrap());
        assert!(!r.is_tombstone(0).unwrap());

        // Sparse vector survives.
        match r.get(&DocId::from("d")).unwrap().unwrap() {
            SegmentRow::Live(d) => {
                let sv = d.sparse_vector.unwrap();
                assert_eq!(sv.indices, vec![3, 17, 42]);
                assert_eq!(sv.values, vec![0.5, 1.5, -0.25]);
            }
            other => panic!("expected live row, got {other:?}"),
        }

        // Direct vector access path.
        assert_eq!(r.vector(3).unwrap(), Some(vec![1.0, -1.0, 0.5]));
        assert_eq!(r.vector(1).unwrap(), None);

        r.verify_all_blocks().unwrap();
    }

    #[test]
    fn rows_materializes_in_id_order() {
        let enc = build_sample();
        let r = SegmentReader::open(enc.bytes).unwrap();
        let rows = r.rows().unwrap();
        let ids: Vec<String> = rows.iter().map(|r| format!("{:?}", r.id())).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        assert_eq!(rows.iter().filter(|r| r.is_tombstone()).count(), 1);
    }

    #[test]
    fn rejects_unsorted_and_duplicate_ids() {
        let mut b = SegmentBuilder::new("ns", "s", 1, 1);
        b.push_live(doc("b", None, 1)).unwrap();
        assert!(matches!(
            b.push_live(doc("a", None, 2)),
            Err(EngineError::InvalidArgument(_))
        ));
        assert!(matches!(
            b.push_live(doc("b", None, 3)),
            Err(EngineError::InvalidArgument(_))
        ));
    }

    #[test]
    fn rejects_inconsistent_vector_dims_and_empty_segments() {
        let mut b = SegmentBuilder::new("ns", "s", 1, 2);
        b.push_live(doc("a", Some(vec![1.0, 2.0]), 1)).unwrap();
        b.push_live(doc("b", Some(vec![1.0, 2.0, 3.0]), 2)).unwrap();
        assert!(matches!(
            b.finish(),
            Err(EngineError::InvalidArgument(_))
        ));

        let empty = SegmentBuilder::new("ns", "s", 1, 1);
        assert!(matches!(
            empty.finish(),
            Err(EngineError::InvalidArgument(_))
        ));
    }

    #[test]
    fn detects_block_bitflip() {
        let enc = build_sample();
        // Flip one byte inside the ids block payload (first block, right
        // after the 16-byte header).
        let mut bytes = enc.bytes.to_vec();
        bytes[HEADER_LEN + 2] ^= 0x40;
        let r = SegmentReader::open(Bytes::from(bytes)).unwrap();
        let err = r.ids().unwrap_err();
        assert!(matches!(err, EngineError::Corruption(_)), "got {err:?}");
    }

    #[test]
    fn detects_truncation_and_bad_magic() {
        let enc = build_sample();

        // Truncated file: trailer magic check fails.
        let truncated = enc.bytes.slice(..enc.bytes.len() - 5);
        assert!(matches!(
            SegmentReader::open(truncated),
            Err(EngineError::Corruption(_))
        ));

        // Corrupted footer.
        let mut bytes = enc.bytes.to_vec();
        let footer_mid = bytes.len() - TRAILER_LEN - 4;
        bytes[footer_mid] ^= 0xFF;
        assert!(matches!(
            SegmentReader::open(Bytes::from(bytes)),
            Err(EngineError::Corruption(_))
        ));

        // Wrong header magic.
        let mut bad_magic = enc.bytes.to_vec();
        bad_magic[0] = b'X';
        assert!(matches!(
            SegmentReader::open(Bytes::from(bad_magic)),
            Err(EngineError::Corruption(_))
        ));
    }

    #[test]
    fn segment_ref_carries_footer_accounting() {
        let enc = build_sample();
        let size = enc.size_bytes();
        let sref = enc.footer.to_segment_ref(size, 0, "ns-a");
        assert_eq!(sref.segment_id, "seg-0001");
        assert_eq!(sref.doc_count, 4);
        assert_eq!(sref.live_doc_count, 3);
        assert_eq!(sref.tombstone_count, 1);
        assert_eq!(sref.size_bytes, size);
        assert_eq!(sref.min_wal_seq, 1);
        assert_eq!(sref.max_wal_seq, 9);
        assert_eq!(sref.owner_namespace, "ns-a");
    }
}
