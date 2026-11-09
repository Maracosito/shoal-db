//! # shoal-query
//!
//! The query engine for Shoal, an object-storage-native vector and full-text
//! search database. This crate contains the pure (storage-agnostic) query
//! path:
//!
//! * dense-vector search — exact kNN baselines and an IVF/centroid ANN index
//!   designed for object-storage-backed retrieval ([`vector`]),
//! * full-text search — tokenizer, persistent inverted index, BM25 ranking
//!   with field boosting ([`text`]),
//! * sparse-vector scoring ([`sparse`]),
//! * the metadata filter engine and attribute/filter indexes ([`filter`]),
//! * score fusion primitives ([`fusion`]),
//! * top-k collection ([`topk`]) and on-disk codecs ([`codec`]),
//! * the request types, query planner, and executor that tie the retrieval
//!   primitives together ([`plan`]),
//! * wire-format request/response types ([`wire`]).
//!
//! The serving layer (API server) and storage layer (WAL, segments,
//! compaction) live in sibling crates; this crate operates over abstract
//! sources so it can be tested without any storage backend.

pub mod codec;
pub mod error;
pub mod filter;
pub mod fusion;
pub mod plan;
pub mod sparse;
pub mod text;
pub mod topk;
pub mod types;
pub mod vector;
pub mod wire;

pub use error::*;
pub use types::*;
