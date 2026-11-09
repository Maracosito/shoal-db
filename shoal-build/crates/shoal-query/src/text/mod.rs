//! Full-text search engine: tokenizer, persistent inverted index segments,
//! and BM25 ranking with field boosting.
//!
//! ## Design
//!
//! Full-text indexes follow the same object-storage-native shape as the
//! vector and filter indexes in this crate:
//!
//! * Each storage segment gets one immutable [`inverted::InvertedSegment`]
//!   blob, written once by the background indexer and referenced from the
//!   namespace's index manifest. Blobs are fetched from object storage and
//!   cached on local disk like any other segment file.
//! * On load, only the term dictionary, per-field statistics, and doc-length
//!   tables are materialized in memory; posting lists stay as compact
//!   delta/varint bytes and are decoded lazily per query term.
//! * Documents are addressed by local `u32` ordinals — the same ordinal space
//!   used by the filter doc-sets — so BM25 results can be intersected with
//!   filter results and joined back to document ids by the executor.
//! * Cross-segment scoring: BM25 statistics (`df`, `avgdl`, `N`) are
//!   per-segment in v1. The executor merges per-segment top-k lists; after
//!   compaction a namespace typically converges to a small number of large
//!   segments, which keeps per-segment statistics close to global ones. This
//!   tradeoff is documented in the architecture guide.
//!
//! [`bm25::search`] is the entry point used by the query planner; the
//! [`tokenizer::Tokenizer`] must be configured identically at index and query
//! time (the configuration is recorded in the namespace's index manifest).

pub mod bm25;
pub mod inverted;
pub mod tokenizer;

pub use bm25::{idf, search as bm25_search, tf_norm, Bm25Params, ScoredDoc};
pub use inverted::{
    FieldInfo, InvertedIndexBuilder, InvertedSegment, PostingsCursor, TextError, SEGMENT_MAGIC,
    SEGMENT_VERSION,
};
pub use tokenizer::{Token, Tokenizer, TokenizerConfig};
