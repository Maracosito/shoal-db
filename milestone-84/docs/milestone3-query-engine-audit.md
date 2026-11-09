# Milestone 3 Acceptance Audit — Query Engine

This document is a traceability audit for Milestone 3 (*Query Engine: vector
search, BM25 full-text, filters, hybrid, planner*). It maps every funded
deliverable in the milestone scope to the concrete files and test suites that
implement it inside `crates/shoal-query`, so reviewers can verify coverage
without reverse-engineering the crate layout.

All code referenced below has already been delivered in this milestone; this
document adds the acceptance mapping only.

## 1. Scope-to-deliverable traceability

| # | Funded deliverable | Implementation | Tests |
|---|--------------------|----------------|-------|
| 1 | Exact kNN baseline with cosine / dot / Euclidean | `src/vector/math.rs` (distance kernels, normalization), `src/vector/exact.rs` (brute-force top-k scan over candidate doc sets) | inline unit tests in `vector/math.rs` and `vector/exact.rs`; exact path is also the ground-truth oracle in `tests/vector_recall.rs` |
| 2 | IVF/centroid-style ANN designed for object-storage-backed retrieval | `src/vector/kmeans.rs` (Lloyd's k-means++ training), `src/vector/ivf.rs` (centroid table + per-centroid posting lists; centroids are a small manifest-resident block, each posting list is an independently addressable, independently fetchable unit so segments can be cached per-list), `src/codec.rs` (versioned, checksummed binary encoding for index blocks) | `tests/vector_recall.rs` |
| 3 | Recall-vs-nprobe tuning | `nprobe` is a per-query runtime parameter on the IVF search path (`src/vector/ivf.rs`); `tests/vector_recall.rs` sweeps `nprobe` against the exact-kNN oracle and asserts recall@k is monotonically usable and crosses the target threshold | `tests/vector_recall.rs` |
| 4 | Tokenizer | `src/text/tokenizer.rs` (Unicode-aware lowercasing tokenizer with configurable stopword handling) | inline unit tests; exercised end-to-end by `tests/bm25_correctness.rs` |
| 5 | Persistent inverted index | `src/text/inverted.rs` (in-memory build + postings with term/doc frequencies and field-level stats) serialized through `src/codec.rs` into segment-storable byte blocks | `tests/bm25_correctness.rs` |
| 6 | BM25 ranking with field boosting | `src/text/bm25.rs` (BM25 with configurable `k1`/`b`, per-field boosts combined at score time) | `tests/bm25_correctness.rs` (hand-computed BM25 scores, IDF edge cases, boost ordering) |
| 7 | Optional sparse-vector scoring path | `src/sparse.rs` (sparse dot-product scoring over (index, weight) pairs) | inline unit tests in `src/sparse.rs` |
| 8 | Metadata filter engine: Eq/NotEq/Gt/Gte/Lt/Lte/In/ContainsAny + And/Or/Not | `src/filter/mod.rs` (filter AST + evaluation), `src/filter/docset.rs` (doc-set algebra: union/intersect/negate over sorted ID sets) | `tests/filter_algebra.rs` (operator semantics, boolean algebra laws, De Morgan checks) |
| 9 | Attribute/filter indexes for fast filtered search | `src/filter/index.rs` (per-attribute value→docset indexes used to resolve predicates without scanning) | `tests/filter_algebra.rs` (index-resolved results equal full-scan results) |
| 10 | Hybrid search: weighted score fusion + reciprocal rank fusion | `src/fusion.rs` (score normalization, weighted fusion, RRF with configurable `k`), `src/plan/fuse.rs` (fusion stage wiring in execution) | inline unit tests in `src/fusion.rs`; end-to-end in `tests/planner_executor.rs` |
| 11 | Multi-query requests | `src/plan/request.rs` (request model supports multiple sub-queries per call), `src/plan/executor.rs` (per-sub-query execution + fusion) | `tests/planner_executor.rs` |
| 12 | Selected-attribute projection and top_k | `src/topk.rs` (bounded max-heap top-k collector), `src/wire.rs` + `src/plan/executor.rs` (attribute projection of result rows) | inline unit tests in `src/topk.rs`; projection assertions in `tests/planner_executor.rs` |
| 13 | Query planner choosing exact / ANN / full-text / filter-index / hybrid paths | `src/plan/planner.rs` (selectivity- and availability-driven plan selection: exact scan for small or highly filtered candidate sets, IVF for large unfiltered/lightly filtered vector queries, inverted-index path for text, hybrid plan when both rankers are requested), `src/plan/mod.rs` | `tests/planner_executor.rs` (asserts chosen plan per request shape and that executed results are correct) |
| 14 | Unit tests: vector math, BM25 correctness, filter algebra, recall@k vs exact kNN | — | `tests/vector_recall.rs`, `tests/bm25_correctness.rs`, `tests/filter_algebra.rs`, `tests/planner_executor.rs`, plus inline `#[cfg(test)]` modules across `vector/`, `text/`, `filter/`, `fusion.rs`, `sparse.rs`, `topk.rs` |

Shared infrastructure delivered this milestone: `src/error.rs` (query-engine
error taxonomy), `src/types.rs` (document/attribute/score types shared across
paths), `src/lib.rs` (public surface).

## 2. Object-storage orientation (design intent for acceptance)

The IVF layout deliberately splits into two artifact classes so that the
serving tier can stay stateless:

- **Centroid block** — small (`nlist × dim × f32`), intended to live in (or
  alongside) the namespace manifest and be held in memory per warm namespace.
- **Posting-list blocks** — one independently encoded, checksummed block per
  centroid via `src/codec.rs`. A query touching `nprobe` lists fetches at most
  `nprobe` blocks; each block is a natural unit for the disk/memory cache
  hierarchy delivered in the caching milestone.

The inverted index and attribute indexes use the same codec framing so they
are storable and re-fetchable as immutable segment files.

## 3. Explicitly out of scope for this milestone (per project plan)

- Wiring `src/wire.rs` request/response types into the HTTP server
  (`shoal-api`) — API-server milestone.
- MinIO-backed restart/reconstruction integration tests for index files —
  indexing/recovery milestone (the codec layer delivered here is what those
  tests will exercise).
- Disk/memory cache implementation, warm/pin endpoints, cold-vs-warm
  benchmarks — caching milestone.

## 4. How to verify

```sh
cargo test -p shoal-query                      # full unit + integration suite
cargo test -p shoal-query --test vector_recall # recall@k: IVF vs exact oracle, nprobe sweep
cargo test -p shoal-query --test bm25_correctness
cargo test -p shoal-query --test filter_algebra
cargo test -p shoal-query --test planner_executor
```

Reviewer checklist before sign-off on a clean machine:

1. Confirm the workspace root `Cargo.toml` includes `crates/shoal-query` in
   `[workspace] members` (a `crates/*` glob already covers it).
2. `cargo test -p shoal-query` passes on current stable Rust.
3. `tests/vector_recall.rs` reports recall@10 meeting its asserted threshold
   at the swept `nprobe` values.
