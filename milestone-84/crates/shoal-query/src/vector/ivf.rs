//! IVF-Flat approximate nearest neighbor index, designed for
//! object-storage-backed retrieval.
//!
//! # Layout
//!
//! The serialized index is a single immutable object with three regions:
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │ header (60 bytes, CRC-protected)                           │
//! │   magic, version, metric, dim, nlist, total,               │
//! │   centroids_offset, directory_offset, lists_offset         │
//! ├────────────────────────────────────────────────────────────┤
//! │ centroids: nlist * dim f32 (LE)            + CRC32         │
//! ├────────────────────────────────────────────────────────────┤
//! │ directory: nlist * {offset u64, byte_len,                  │
//! │            count, crc u32}                 + CRC32         │
//! ├────────────────────────────────────────────────────────────┤
//! │ posting list 0 │ posting list 1 │ ... │ posting list n-1   │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! A query node only needs the **prefix** (`header + centroids +
//! directory`, i.e. bytes `[0, lists_offset)`) to plan a search — this is
//! what gets pinned in the memory cache. It then range-GETs exactly the
//! `nprobe` posting lists it probes; each list is independently
//! CRC-checked and cacheable on local SSD. Vectors are stored unquantized
//! ("Flat"), so a full probe of all lists is *exactly* equivalent to
//! brute-force search.
//!
//! For [`Metric::Cosine`], vectors are L2-normalized at build time and the
//! query is normalized at search time, so scan scoring reduces to a dot
//! product.

use roaring::RoaringBitmap;

use crate::error::{QueryError, Result};
use crate::topk::{Neighbor, TopK};
use crate::types::DocOrd;
use crate::vector::kmeans::{self, KMeansConfig};
use crate::vector::math::{self, Metric};

/// File magic for serialized IVF indexes.
pub const IVF_MAGIC: &[u8; 8] = b"SHOALIVF";
/// Current format version.
pub const IVF_VERSION: u32 = 1;
/// Fixed header length in bytes (including its CRC).
pub const IVF_HEADER_LEN: usize = 60;
/// Bytes per directory entry.
pub const IVF_DIR_ENTRY_LEN: usize = 20;

/// Build-time configuration.
#[derive(Debug, Clone)]
pub struct IvfConfig {
    /// Number of inverted lists; `0` selects `sqrt(n)` clamped to
    /// `[1, 4096]`.
    pub nlist: usize,
    /// Maximum number of vectors sampled for k-means training.
    pub train_sample: usize,
    /// Lloyd iterations for centroid training.
    pub kmeans_iters: usize,
    /// RNG seed for deterministic builds.
    pub seed: u64,
}

impl Default for IvfConfig {
    fn default() -> Self {
        IvfConfig {
            nlist: 0,
            train_sample: 50_000,
            kmeans_iters: 12,
            seed: 0x5EED_1F1F,
        }
    }
}

/// One inverted list: ordinals plus their (flat, row-major) vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct PostingList {
    pub ords: Vec<DocOrd>,
    pub vectors: Vec<f32>,
}

/// Directory entry describing where a posting list lives in the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListDirEntry {
    /// Absolute byte offset of the list blob within the object.
    pub offset: u64,
    /// Length of the blob in bytes.
    pub byte_len: u32,
    /// Number of vectors in the list.
    pub count: u32,
    /// CRC32 of the blob.
    pub crc: u32,
}

/// Parsed fixed header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IvfHeader {
    pub metric: Metric,
    pub dim: usize,
    pub nlist: usize,
    pub total: u64,
    pub centroids_offset: u64,
    pub directory_offset: u64,
    pub lists_offset: u64,
}

impl IvfHeader {
    /// Number of prefix bytes (header + centroids + directory) a reader
    /// must fetch before it can plan probes.
    pub fn prefix_len(&self) -> usize {
        self.lists_offset as usize
    }

    /// Parse and validate the fixed header from the first
    /// [`IVF_HEADER_LEN`] bytes of the object.
    pub fn from_bytes(bytes: &[u8]) -> Result<IvfHeader> {
        if bytes.len() < IVF_HEADER_LEN {
            return Err(QueryError::InvalidIndexFormat(format!(
                "ivf header too short: {} < {IVF_HEADER_LEN}",
                bytes.len()
            )));
        }
        if &bytes[0..8] != IVF_MAGIC {
            return Err(QueryError::InvalidIndexFormat("bad ivf magic".into()));
        }
        let stored_crc = read_u32(bytes, 56);
        let computed = crc32fast::hash(&bytes[0..56]);
        if stored_crc != computed {
            return Err(QueryError::ChecksumMismatch {
                context: "ivf header",
                stored: stored_crc,
                computed,
            });
        }
        let version = read_u32(bytes, 8);
        if version != IVF_VERSION {
            return Err(QueryError::InvalidIndexFormat(format!(
                "unsupported ivf version {version}"
            )));
        }
        let metric = Metric::from_u8(bytes[12])?;
        let dim = read_u32(bytes, 16) as usize;
        let nlist = read_u32(bytes, 20) as usize;
        let total = read_u64(bytes, 24);
        let centroids_offset = read_u64(bytes, 32);
        let directory_offset = read_u64(bytes, 40);
        let lists_offset = read_u64(bytes, 48);
        if dim == 0 || nlist == 0 {
            return Err(QueryError::InvalidIndexFormat(
                "ivf header has zero dim or nlist".into(),
            ));
        }
        // Cross-check the offsets against dim/nlist so a corrupted header
        // that somehow passed CRC cannot send us out of bounds.
        let expect_cent = IVF_HEADER_LEN as u64;
        let expect_dir = expect_cent + (nlist as u64 * dim as u64 * 4) + 4;
        let expect_lists = expect_dir + (nlist as u64 * IVF_DIR_ENTRY_LEN as u64) + 4;
        if centroids_offset != expect_cent
            || directory_offset != expect_dir
            || lists_offset != expect_lists
        {
            return Err(QueryError::InvalidIndexFormat(
                "ivf header offsets inconsistent with dim/nlist".into(),
            ));
        }
        Ok(IvfHeader {
            metric,
            dim,
            nlist,
            total,
            centroids_offset,
            directory_offset,
            lists_offset,
        })
    }
}

/// The cache-resident prefix of an IVF index: everything needed to rank
/// centroids and locate posting lists, without the lists themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct IvfMeta {
    pub header: IvfHeader,
    /// Row-major `nlist * dim` centroid matrix.
    pub centroids: Vec<f32>,
    pub directory: Vec<ListDirEntry>,
}

impl IvfMeta {
    /// Parse the prefix region of a serialized index. `bytes` must contain
    /// at least `header.prefix_len()` bytes (a whole object also works).
    pub fn from_prefix(bytes: &[u8]) -> Result<IvfMeta> {
        let header = IvfHeader::from_bytes(bytes)?;
        let need = header.prefix_len();
        if bytes.len() < need {
            return Err(QueryError::InvalidIndexFormat(format!(
                "ivf prefix too short: {} < {need}",
                bytes.len()
            )));
        }
        // Centroids.
        let cstart = header.centroids_offset as usize;
        let clen = header.nlist * header.dim * 4;
        let cbytes = &bytes[cstart..cstart + clen];
        let stored = read_u32(bytes, cstart + clen);
        let computed = crc32fast::hash(cbytes);
        if stored != computed {
            return Err(QueryError::ChecksumMismatch {
                context: "ivf centroids",
                stored,
                computed,
            });
        }
        let centroids = f32_slice_from_le(cbytes);
        // Directory.
        let dstart = header.directory_offset as usize;
        let dlen = header.nlist * IVF_DIR_ENTRY_LEN;
        let dbytes = &bytes[dstart..dstart + dlen];
        let stored = read_u32(bytes, dstart + dlen);
        let computed = crc32fast::hash(dbytes);
        if stored != computed {
            return Err(QueryError::ChecksumMismatch {
                context: "ivf directory",
                stored,
                computed,
            });
        }
        let mut directory = Vec::with_capacity(header.nlist);
        for i in 0..header.nlist {
            let base = i * IVF_DIR_ENTRY_LEN;
            directory.push(ListDirEntry {
                offset: read_u64(dbytes, base),
                byte_len: read_u32(dbytes, base + 8),
                count: read_u32(dbytes, base + 12),
                crc: read_u32(dbytes, base + 16),
            });
        }
        Ok(IvfMeta {
            header,
            centroids,
            directory,
        })
    }
}

/// Decode and CRC-verify a posting-list blob fetched from storage.
pub fn decode_posting_list(blob: &[u8], dim: usize, entry: &ListDirEntry) -> Result<PostingList> {
    if blob.len() != entry.byte_len as usize {
        return Err(QueryError::InvalidIndexFormat(format!(
            "posting list blob length {} != directory byte_len {}",
            blob.len(),
            entry.byte_len
        )));
    }
    let computed = crc32fast::hash(blob);
    if computed != entry.crc {
        return Err(QueryError::ChecksumMismatch {
            context: "ivf posting list",
            stored: entry.crc,
            computed,
        });
    }
    if blob.len() < 4 {
        return Err(QueryError::InvalidIndexFormat("posting list too short".into()));
    }
    let count = read_u32(blob, 0) as usize;
    if count != entry.count as usize {
        return Err(QueryError::InvalidIndexFormat(format!(
            "posting list count {count} != directory count {}",
            entry.count
        )));
    }
    let expected_len = 4 + count * 4 + count * dim * 4;
    if blob.len() != expected_len {
        return Err(QueryError::InvalidIndexFormat(format!(
            "posting list blob length {} != expected {expected_len}",
            blob.len()
        )));
    }
    let mut ords = Vec::with_capacity(count);
    for i in 0..count {
        ords.push(read_u32(blob, 4 + i * 4));
    }
    let vstart = 4 + count * 4;
    let vectors = f32_slice_from_le(&blob[vstart..]);
    Ok(PostingList { ords, vectors })
}

/// Abstraction over how posting lists are obtained: from object storage
/// (range GET), from the local disk cache, or from memory. Implementations
/// should serve decoded, CRC-verified lists.
pub trait PostingFetch {
    fn fetch_list(&self, list_id: u32) -> Result<PostingList>;
}

/// Fully in-memory IVF-Flat index.
#[derive(Debug, Clone, PartialEq)]
pub struct IvfIndex {
    pub metric: Metric,
    pub dim: usize,
    pub nlist: usize,
    /// Row-major `nlist * dim` centroid matrix.
    pub centroids: Vec<f32>,
    pub lists: Vec<PostingList>,
    pub total: u64,
}

impl IvfIndex {
    /// Build an index from a flat vector block: `ords[i]`'s vector is
    /// `vectors[i*dim .. (i+1)*dim]`.
    pub fn build(
        metric: Metric,
        dim: usize,
        ords: &[DocOrd],
        vectors: &[f32],
        cfg: &IvfConfig,
    ) -> Result<IvfIndex> {
        if dim == 0 {
            return Err(QueryError::InvalidArgument("ivf: dim must be > 0".into()));
        }
        if vectors.len() != ords.len() * dim {
            return Err(QueryError::InvalidArgument(format!(
                "ivf: {} ords * dim {} != {} floats",
                ords.len(),
                dim,
                vectors.len()
            )));
        }
        let n = ords.len();

        // Empty namespace: emit a minimal valid index with one empty list.
        if n == 0 {
            return Ok(IvfIndex {
                metric,
                dim,
                nlist: 1,
                centroids: vec![0.0; dim],
                lists: vec![PostingList { ords: vec![], vectors: vec![] }],
                total: 0,
            });
        }

        // For cosine, store normalized vectors so scans are dot products.
        let owned;
        let data: &[f32] = if metric == Metric::Cosine {
            let mut d = vectors.to_vec();
            for i in 0..n {
                math::normalize_in_place(&mut d[i * dim..(i + 1) * dim]);
            }
            owned = d;
            &owned
        } else {
            vectors
        };

        let nlist = if cfg.nlist > 0 {
            cfg.nlist.min(n)
        } else {
            ((n as f64).sqrt().round() as usize).clamp(1, 4096).min(n)
        };

        // Sample training points deterministically.
        let sample = cfg.train_sample.max(nlist).min(n);
        let train_buf;
        let train_data: &[f32] = if sample < n {
            use rand::seq::SliceRandom;
            use rand::SeedableRng;
            let mut rng = rand::rngs::StdRng::seed_from_u64(cfg.seed);
            let mut idx: Vec<usize> = (0..n).collect();
            idx.shuffle(&mut rng);
            idx.truncate(sample);
            let mut buf = Vec::with_capacity(sample * dim);
            for &i in &idx {
                buf.extend_from_slice(&data[i * dim..(i + 1) * dim]);
            }
            train_buf = buf;
            &train_buf
        } else {
            data
        };

        let km = kmeans::train(
            train_data,
            dim,
            &KMeansConfig {
                k: nlist,
                max_iters: cfg.kmeans_iters,
                seed: cfg.seed,
            },
        )?;
        let nlist = km.k; // may be clamped by the sample size
        let centroids = km.centroids;

        // Assign every vector to its nearest centroid (L2, the k-means
        // geometry — standard for IVF regardless of search metric).
        let mut lists: Vec<PostingList> = (0..nlist)
            .map(|_| PostingList { ords: vec![], vectors: vec![] })
            .collect();
        for i in 0..n {
            let v = &data[i * dim..(i + 1) * dim];
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for c in 0..nlist {
                let d = math::l2_sq(v, &centroids[c * dim..(c + 1) * dim]);
                if d < best_d {
                    best_d = d;
                    best = c;
                }
            }
            lists[best].ords.push(ords[i]);
            lists[best].vectors.extend_from_slice(v);
        }

        Ok(IvfIndex {
            metric,
            dim,
            nlist,
            centroids,
            lists,
            total: n as u64,
        })
    }

    /// A reasonable starting `nprobe` for a given recall appetite; callers
    /// tune from here using the recall harness.
    pub fn suggest_nprobe(&self) -> usize {
        (self.nlist / 8).max(1)
    }

    /// Search the in-memory index.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        nprobe: usize,
        allow: Option<&RoaringBitmap>,
    ) -> Result<Vec<Neighbor>> {
        let q = prepare_query(self.metric, query, self.dim)?;
        let probes = rank_probes(&self.centroids, self.dim, self.nlist, self.metric, &q, nprobe);
        let mut topk = TopK::new(k);
        for p in probes {
            scan_list(&self.lists[p as usize], self.metric, &q, self.dim, allow, &mut topk)?;
        }
        Ok(topk.into_sorted())
    }

    /// Serialize to the on-object format described in the module docs.
    pub fn to_bytes(&self) -> Vec<u8> {
        let dim = self.dim;
        let nlist = self.nlist;
        let cent_len = nlist * dim * 4 + 4;
        let dir_len = nlist * IVF_DIR_ENTRY_LEN + 4;
        let centroids_offset = IVF_HEADER_LEN as u64;
        let directory_offset = centroids_offset + cent_len as u64;
        let lists_offset = directory_offset + dir_len as u64;

        // Encode list blobs first to learn their sizes.
        let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(nlist);
        let mut entries: Vec<ListDirEntry> = Vec::with_capacity(nlist);
        let mut offset = lists_offset;
        for list in &self.lists {
            let mut blob = Vec::with_capacity(4 + list.ords.len() * (4 + dim * 4));
            put_u32(&mut blob, list.ords.len() as u32);
            for &o in &list.ords {
                put_u32(&mut blob, o);
            }
            put_f32_slice(&mut blob, &list.vectors);
            let crc = crc32fast::hash(&blob);
            entries.push(ListDirEntry {
                offset,
                byte_len: blob.len() as u32,
                count: list.ords.len() as u32,
                crc,
            });
            offset += blob.len() as u64;
            blobs.push(blob);
        }

        let mut out = Vec::with_capacity(offset as usize);
        // Header.
        out.extend_from_slice(IVF_MAGIC);
        put_u32(&mut out, IVF_VERSION);
        out.push(self.metric.as_u8());
        out.extend_from_slice(&[0u8; 3]);
        put_u32(&mut out, dim as u32);
        put_u32(&mut out, nlist as u32);
        put_u64(&mut out, self.total);
        put_u64(&mut out, centroids_offset);
        put_u64(&mut out, directory_offset);
        put_u64(&mut out, lists_offset);
        let hcrc = crc32fast::hash(&out[0..56]);
        put_u32(&mut out, hcrc);
        // Centroids.
        let cstart = out.len();
        put_f32_slice(&mut out, &self.centroids);
        let ccrc = crc32fast::hash(&out[cstart..]);
        put_u32(&mut out, ccrc);
        // Directory.
        let dstart = out.len();
        for e in &entries {
            put_u64(&mut out, e.offset);
            put_u32(&mut out, e.byte_len);
            put_u32(&mut out, e.count);
            put_u32(&mut out, e.crc);
        }
        let dcrc = crc32fast::hash(&out[dstart..]);
        put_u32(&mut out, dcrc);
        // Lists.
        for blob in blobs {
            out.extend_from_slice(&blob);
        }
        out
    }

    /// Deserialize a whole object back into an in-memory index, verifying
    /// every checksum.
    pub fn from_bytes(bytes: &[u8]) -> Result<IvfIndex> {
        let meta = IvfMeta::from_prefix(bytes)?;
        let mut lists = Vec::with_capacity(meta.header.nlist);
        for entry in &meta.directory {
            let start = entry.offset as usize;
            let end = start
                .checked_add(entry.byte_len as usize)
                .ok_or_else(|| QueryError::InvalidIndexFormat("list offset overflow".into()))?;
            if end > bytes.len() {
                return Err(QueryError::InvalidIndexFormat(format!(
                    "posting list extends past object end ({end} > {})",
                    bytes.len()
                )));
            }
            lists.push(decode_posting_list(&bytes[start..end], meta.header.dim, entry)?);
        }
        Ok(IvfIndex {
            metric: meta.header.metric,
            dim: meta.header.dim,
            nlist: meta.header.nlist,
            centroids: meta.centroids,
            lists,
            total: meta.header.total,
        })
    }
}

/// Storage-backed search: rank centroids using the cached [`IvfMeta`]
/// prefix, then fetch only the probed posting lists through `fetcher`.
pub fn search_with_meta<F: PostingFetch>(
    meta: &IvfMeta,
    fetcher: &F,
    query: &[f32],
    k: usize,
    nprobe: usize,
    allow: Option<&RoaringBitmap>,
) -> Result<Vec<Neighbor>> {
    let h = &meta.header;
    let q = prepare_query(h.metric, query, h.dim)?;
    let probes = rank_probes(&meta.centroids, h.dim, h.nlist, h.metric, &q, nprobe);
    let mut topk = TopK::new(k);
    for p in probes {
        let list = fetcher.fetch_list(p)?;
        scan_list(&list, h.metric, &q, h.dim, allow, &mut topk)?;
    }
    Ok(topk.into_sorted())
}

/// Fraction of the exact top-k that the approximate top-k recovered.
pub fn recall_at_k(approx: &[Neighbor], exact: &[Neighbor], k: usize) -> f64 {
    use std::collections::HashSet;
    let truth: HashSet<DocOrd> = exact.iter().take(k).map(|n| n.ord).collect();
    if truth.is_empty() {
        return 1.0;
    }
    let hits = approx
        .iter()
        .take(k)
        .filter(|n| truth.contains(&n.ord))
        .count();
    hits as f64 / truth.len() as f64
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn prepare_query(metric: Metric, query: &[f32], dim: usize) -> Result<Vec<f32>> {
    if query.len() != dim {
        return Err(QueryError::DimensionMismatch {
            expected: dim,
            got: query.len(),
        });
    }
    Ok(if metric == Metric::Cosine {
        math::normalized(query)
    } else {
        query.to_vec()
    })
}

/// Rank centroids by metric score (descending) and return the top `nprobe`
/// list IDs.
fn rank_probes(
    centroids: &[f32],
    dim: usize,
    nlist: usize,
    metric: Metric,
    q: &[f32],
    nprobe: usize,
) -> Vec<u32> {
    let mut scored: Vec<(f32, u32)> = (0..nlist)
        .map(|c| {
            (
                math::score(metric, q, &centroids[c * dim..(c + 1) * dim]),
                c as u32,
            )
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(nprobe.clamp(1, nlist));
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Score a posting list into the top-k accumulator. For cosine the stored
/// vectors and query are normalized, so scoring is a plain dot product.
fn scan_list(
    list: &PostingList,
    metric: Metric,
    q: &[f32],
    dim: usize,
    allow: Option<&RoaringBitmap>,
    topk: &mut TopK,
) -> Result<()> {
    if list.vectors.len() != list.ords.len() * dim {
        return Err(QueryError::InvalidIndexFormat(format!(
            "posting list internal mismatch: {} ords * dim {dim} != {} floats",
            list.ords.len(),
            list.vectors.len()
        )));
    }
    for (i, &ord) in list.ords.iter().enumerate() {
        if let Some(bm) = allow {
            if !bm.contains(ord) {
                continue;
            }
        }
        let v = &list.vectors[i * dim..(i + 1) * dim];
        let score = match metric {
            Metric::Cosine | Metric::Dot => math::dot(q, v),
            Metric::Euclidean => -math::l2_sq(q, v),
        };
        topk.push(Neighbor { ord, score });
    }
    Ok(())
}

// --- little-endian byte helpers --------------------------------------------

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_f32_slice(buf: &mut Vec<u8>, s: &[f32]) {
    buf.reserve(s.len() * 4);
    for &x in s {
        buf.extend_from_slice(&x.to_le_bytes());
    }
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

fn f32_slice_from_le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    fn small_index(metric: Metric) -> (IvfIndex, Vec<DocOrd>, Vec<f32>, usize) {
        let mut rng = StdRng::seed_from_u64(11);
        let dim = 12;
        let n = 400;
        let ords: Vec<DocOrd> = (0..n as u32).collect();
        let data: Vec<f32> = (0..n * dim).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
        let cfg = IvfConfig { nlist: 16, ..Default::default() };
        let idx = IvfIndex::build(metric, dim, &ords, &data, &cfg).unwrap();
        (idx, ords, data, dim)
    }

    #[test]
    fn serialization_round_trip_is_lossless() {
        for metric in [Metric::Cosine, Metric::Dot, Metric::Euclidean] {
            let (idx, ..) = small_index(metric);
            let bytes = idx.to_bytes();
            let back = IvfIndex::from_bytes(&bytes).unwrap();
            assert_eq!(idx, back, "metric {metric:?}");
        }
    }

    #[test]
    fn prefix_parse_matches_full_parse() {
        let (idx, ..) = small_index(Metric::Euclidean);
        let bytes = idx.to_bytes();
        let header = IvfHeader::from_bytes(&bytes).unwrap();
        // Parsing only the prefix slice must work.
        let meta = IvfMeta::from_prefix(&bytes[..header.prefix_len()]).unwrap();
        assert_eq!(meta.header.nlist, idx.nlist);
        assert_eq!(meta.centroids, idx.centroids);
        let total: u64 = meta.directory.iter().map(|e| e.count as u64).sum();
        assert_eq!(total, idx.total);
    }

    #[test]
    fn corruption_is_detected() {
        let (idx, ..) = small_index(Metric::Dot);
        let bytes = idx.to_bytes();

        // Flip a byte inside the centroid block.
        let mut bad = bytes.clone();
        bad[IVF_HEADER_LEN + 5] ^= 0xFF;
        assert!(matches!(
            IvfIndex::from_bytes(&bad),
            Err(QueryError::ChecksumMismatch { context: "ivf centroids", .. })
        ));

        // Flip a byte inside the header.
        let mut bad = bytes.clone();
        bad[20] ^= 0x01;
        assert!(IvfIndex::from_bytes(&bad).is_err());

        // Flip a byte inside the first posting list.
        let meta = IvfMeta::from_prefix(&bytes).unwrap();
        let first = meta
            .directory
            .iter()
            .find(|e| e.byte_len > 4)
            .expect("some non-empty list");
        let mut bad = bytes.clone();
        bad[first.offset as usize + 4] ^= 0xFF;
        assert!(matches!(
            IvfIndex::from_bytes(&bad),
            Err(QueryError::ChecksumMismatch { context: "ivf posting list", .. })
        ));

        // Truncated header.
        assert!(IvfHeader::from_bytes(&bytes[..30]).is_err());
        // Bad magic.
        let mut bad = bytes;
        bad[0] = b'X';
        assert!(IvfHeader::from_bytes(&bad).is_err());
    }

    #[test]
    fn empty_index_is_valid_and_searchable() {
        let idx = IvfIndex::build(Metric::Cosine, 8, &[], &[], &IvfConfig::default()).unwrap();
        assert_eq!(idx.total, 0);
        let out = idx.search(&[1.0; 8], 5, 4, None).unwrap();
        assert!(out.is_empty());
        let back = IvfIndex::from_bytes(&idx.to_bytes()).unwrap();
        assert_eq!(idx, back);
    }

    #[test]
    fn rejects_query_dimension_mismatch() {
        let (idx, ..) = small_index(Metric::Euclidean);
        assert!(matches!(
            idx.search(&[1.0, 2.0], 5, 4, None),
            Err(QueryError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn every_vector_lands_in_exactly_one_list() {
        let (idx, ords, ..) = small_index(Metric::Euclidean);
        let mut seen: Vec<DocOrd> = idx.lists.iter().flat_map(|l| l.ords.clone()).collect();
        seen.sort_unstable();
        assert_eq!(seen, ords);
    }
}
