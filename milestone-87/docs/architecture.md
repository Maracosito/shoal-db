# Lagoon Architecture Guide

This document explains how Lagoon stores data, serves queries, stays consistent,
and the tradeoffs behind those choices. Lagoon is an object-storage-native
vector + full-text search database: **object storage is the durable source of
truth**, compute nodes are stateless, and local SSD/memory caches make warm
queries fast.

> Lagoon is an original, clean-room project. It is *inspired by* the public
> product ideas of object-storage-native search engines, but shares no code,
> formats, branding, or documentation with any of them.

---

## 1. Design Principles

1. **Object storage is the database.** Every byte needed to reconstruct a
   namespace lives in an S3-compatible bucket (or a local directory in
   filesystem mode). A compute node can be destroyed at any time without data
   loss.
2. **Stateless compute.** API servers and indexer workers hold only caches.
   Scaling out means starting more processes pointed at the same bucket.
3. **Immutable files, mutable pointers.** WAL chunks and index segments are
   immutable once written. The only mutable object per namespace is a small
   manifest pointer, updated with a compare-and-swap (CAS) operation. This is
   what makes copy-on-write branching cheap and recovery simple.
4. **Cheap when cold, fast when warm.** A namespace that nobody queries costs
   only object-storage bytes. The first query after a cold start pays object
   storage latency; subsequent queries are served from local disk and memory.
5. **Honest consistency.** Single-node deployments give read-your-writes.
   Multi-node deployments are eventually consistent for reads, with a bounded
   staleness window. This is documented, not hidden (see §8).

---

## 2. System Components

```
            ┌────────────┐     HTTP/JSON      ┌──────────────────┐
  clients ─▶│ API server │◀──────────────────▶│  Python / TS SDK │
            │ (stateless)│                    └──────────────────┘
            └─────┬──────┘
                  │ reads: manifests, segments (via cache)
                  │ writes: WAL chunks, manifest CAS
                  ▼
            ┌────────────┐    background      ┌──────────────────┐
            │  Object    │◀──────────────────▶│ Indexer / worker │
            │  storage   │  flush, compact,   │ (stateless)      │
            │ (S3/MinIO/ │  index build, GC   └──────────────────┘
            │  local fs) │
            └────────────┘
```

- **API server** (`lagoon-server`): authenticates requests, appends writes to
  the WAL, executes queries against cached segments, exposes metrics.
- **Indexer worker** (`lagoon-worker`, can also run embedded in the server in
  single-process mode): flushes WAL data into immutable segments, builds
  vector/full-text/attribute indexes, compacts small segments, garbage-collects
  unreferenced files.
- **Object storage**: any S3-compatible store (AWS S3, MinIO, R2, GCS via
  interoperability), or a local filesystem directory for tests and development.
- **Local cache**: a directory on the node's SSD plus an in-process memory
  cache. Both are disposable.

---

## 3. Storage Layout

Each namespace owns a prefix in the bucket. All files under `wal/` and
`segments/` are immutable; `manifest/CURRENT` is the single mutable pointer.

```
<root>/<org>/<project>/<namespace>/
├── manifest/
│   ├── CURRENT                  # tiny file: name of the live manifest (CAS-updated)
│   ├── MANIFEST-000041.json     # immutable manifest generations
│   └── MANIFEST-000042.json
├── wal/
│   ├── 0000000000000117.wal     # immutable WAL chunks, one per accepted batch
│   └── 0000000000000118.wal
├── segments/
│   └── seg-01HX.../             # one directory per immutable segment
│       ├── docs.rows            # row-oriented document store (id → doc)
│       ├── attrs.cols           # column-oriented attribute store + zone maps
│       ├── vectors.ivf          # IVF vector index: centroids + posting lists
│       ├── fulltext.fts         # inverted index blocks + BM25 statistics
│       ├── tombstones.del       # deleted-ID bitmap for this segment
│       └── segment.meta         # checksums, doc count, schema, stats
└── refs/
    └── branch.json              # branch lineage: parent namespace + base manifest
```

### 3.1 The manifest

The manifest is the namespace's commit log head. A manifest generation lists:

- the schema (attribute types, vector dimensions, distance metric, FTS fields),
- every live segment (ID, doc count, min/max attribute stats, file checksums),
- the WAL chunks not yet flushed into segments (the "tail"),
- the last applied WAL sequence number,
- branch metadata (parent namespace and the parent manifest generation it
  branched from, if any),
- a monotonically increasing generation number.

Commits work as follows:

1. Write `MANIFEST-<gen+1>.json` (immutable, idempotent — keyed by generation).
2. CAS `manifest/CURRENT` from `<gen>` to `<gen+1>` using a conditional PUT
   (`If-Match` on ETag where the store supports it; an atomic-rename protocol
   in filesystem mode; a single-writer lease as fallback for stores without
   conditional writes).
3. If the CAS fails, another writer won; re-read, rebase, retry.

Because segments and WAL chunks are immutable and content-addressed by unique
IDs, a failed commit leaves only unreferenced garbage, which GC removes later.
There is never a window where `CURRENT` points at missing or partial data: data
files are always fully written **before** the manifest that references them.

### 3.2 WAL chunks

Each accepted write batch becomes one WAL chunk:

```
header  : magic "LGWL", version, namespace id, sequence number, record count
records : length-prefixed records, each one of
            UPSERT  { id, vector?, sparse?, text fields?, attributes? }
            PATCH   { id, partial attributes / fields }
            DELETE  { id }
            DELETE_BY_FILTER { filter AST }
footer  : CRC32C of the body, record count (redundant, for truncation detection)
```

A write is acknowledged to the client **only after** the WAL chunk PUT succeeds
and the manifest tail is advanced. Chunks carry the client's optional
`Idempotency-Key`; replaying the same key returns the original result instead
of appending a duplicate chunk.

### 3.3 Segments

The indexer flushes the WAL tail into segments. A segment is a self-contained,
immutable, queryable unit:

- **`docs.rows`** — row store for document retrieval and exports: a sorted
  block file of `(id, serialized document)` with a sparse index for point
  lookups.
- **`attrs.cols`** — columnar attribute storage with per-block zone maps
  (min/max, null count, distinct-count estimate) used for filter pruning, plus
  optional per-column value indexes (sorted value → row-id postings) for
  high-selectivity equality/`In` filters.
- **`vectors.ivf`** — IVF-Flat index: k-means centroids (trained on a sample,
  retrained at compaction), followed by per-centroid posting lists of
  `(row id, quantized or raw vector)`. Centroids are small and always cached;
  posting lists are fetched lazily per `nprobe`.
- **`fulltext.fts`** — inverted index: term dictionary (FST-style prefix-
  compressed), postings with positions and field IDs, per-field length norms,
  and corpus statistics for BM25.
- **`tombstones.del`** — a roaring bitmap of row IDs deleted *after* the
  segment was written. Deletes never rewrite segments; they add tombstones,
  which compaction eventually folds in.

All files start with a magic number + format version and end with a CRC32C
footer. Readers verify checksums on first load; a corrupt file is treated as a
cache miss and re-fetched, and a corrupt object-storage copy is surfaced as a
repairable error (`lagoon repair` can rebuild segments from the WAL/source
segments when possible).

---

## 4. Write Path

```
client batch ─▶ validate + assign sequence ─▶ PUT wal/<seq>.wal
            ─▶ commit manifest (tail += chunk) ─▶ ack client
                                  │
                  (async) indexer │ flush tail ─▶ build segment files
                                  ▼
                        PUT segments/seg-*/… ─▶ commit manifest
                        (tail -= chunks, segments += seg)
```

Key properties:

- **Durability**: acked writes exist in object storage. A node crash after the
  ack loses nothing — recovery is just reading `CURRENT`.
- **Read-your-writes (single node)**: the WAL tail is kept in an in-memory
  *memtable* on the node that accepted the write; queries merge memtable
  results (exact scan — tails are small) with segment results.
- **Batching**: clients should batch upserts (the SDKs default to batches of
  1,000). Both row-oriented (`documents: [...]`) and column-oriented
  (`columns: {id: [...], vector: [...], ...}`) payloads are accepted; columnar
  payloads are cheaper to parse for wide, homogeneous batches.
- **Conditional writes**: upserts may carry `if_version` (per-document
  optimistic concurrency) or namespace-level `if_manifest_generation`.
- **Idempotency**: `Idempotency-Key` headers deduplicate retried batches for
  24 hours.

### 4.1 Compaction

Flushing produces many small segments; compaction merges them:

- **Tiering**: segments are grouped into size tiers; when a tier accumulates
  more than `compaction.fanin` (default 8) segments, they merge into the next
  tier.
- During a merge, tombstones are applied, duplicate IDs resolved
  (latest-sequence wins), IVF centroids retrained if the merged segment is
  significantly larger, and BM25 statistics recomputed.
- The merge commits by manifest CAS: new segment in, old segments out, in one
  atomic pointer swap. Readers holding the old manifest keep working — old
  segments are immutable and are only GC'd after a grace period **and** after
  confirming no manifest generation or branch still references them.

---

## 5. Query Path

```
HTTP query ─▶ auth ─▶ resolve manifest (cached, TTL) ─▶ plan ─▶ execute per segment
                                                              ─▶ merge + memtable
                                                              ─▶ fuse (hybrid)
                                                              ─▶ top_k + attributes
```

### 5.1 Planner

The planner chooses an execution strategy per segment from:

| Strategy | When chosen |
|---|---|
| **Exact kNN scan** | namespace/segment below `ann_threshold` docs (default 10k), or filter selectivity estimate < `exact_filter_cutoff` (default 5%), or `consistency: "strong_recall"` requested |
| **IVF probe** | dense vector query over larger segments; `nprobe` scales with `top_k` and a recall target |
| **Inverted-index BM25** | full-text query; term postings intersected/unioned block-at-a-time with WAND-style skipping |
| **Filter-index lookup** | high-selectivity `Eq`/`In` on indexed attributes resolves to a row-id bitmap before any scoring |
| **Zone-map pruning** | range filters skip whole blocks/segments via min/max stats |

Filters always execute as **pre-filters**: a candidate bitmap is computed
first, then vector/text scoring only touches surviving rows. This keeps
filtered recall exact rather than truncating post-hoc.

### 5.2 Hybrid search

A query may carry multiple *rank signals* (dense vector, sparse vector, BM25
text). Each produces a ranked list; lists are fused by:

- **Reciprocal Rank Fusion** (default): `score = Σ 1/(rrf_k + rank_i)`,
  `rrf_k = 60` by default.
- **Weighted score fusion**: per-signal min-max normalized scores combined with
  user weights.

Multi-query requests run several independent queries in one HTTP call and
share manifest/segment loading.

### 5.3 Distance metrics

Cosine, dot product, and Euclidean are supported. Cosine vectors are
normalized at ingest (the raw norm is stored as an attribute so original
vectors are recoverable on export).

---

## 6. Cache Hierarchy

| Layer | Contents | Eviction |
|---|---|---|
| **Memory** | manifests, IVF centroids, term dictionaries, hot postings/columns blocks | LRU by bytes (`cache.memory_bytes`) |
| **Local disk** | whole segment files | LRU by bytes (`cache.disk_bytes`), checksum-verified on read |
| **Object storage** | everything | never (source of truth) |

- **Warm endpoint** (`POST /v1/namespaces/{ns}/warm`) prefetches the manifest,
  centroids, dictionaries, and optionally all segment files.
- **Pinning** (`POST /v1/namespaces/{ns}/pin`) exempts a namespace's cached
  files from eviction (bounded by `cache.max_pinned_bytes`).
- Every cache layer exports hit/miss counters
  (`lagoon_cache_hits_total{layer=...}`) — see the benchmark guide for measured
  cold/warm behavior.

---

## 7. Namespace Branching (Copy-on-Write)

`POST /v1/namespaces/{src}/branch` creates a new namespace in O(1) data terms:

1. Read the source's current manifest generation `G`.
2. Write the branch's `refs/branch.json` recording `(parent = src, base = G)`.
3. Write the branch's first manifest, whose segment list **references the
   parent's segment files by full path** — no data is copied.
4. CAS the branch's `CURRENT`.

Consequences:

- **Isolation both ways**: branch writes create branch-local WAL/segments;
  source writes advance the source manifest, which the branch never re-reads
  (it pinned generation `G`).
- **Multi-level branches** work the same way — a manifest may reference
  segments from any ancestor.
- **Deletion safety**: GC is a mark-and-sweep across *all* namespaces in the
  org: a segment file is deletable only if no live manifest generation of any
  namespace references it. Deleting a branch deletes its own files and its
  refs, never shared ancestors' files.
- `copy` is the eager variant: same API shape, but physically duplicates
  files, for when you want to delete the source afterwards.

---

## 8. Consistency Model

**Single-node (server + embedded indexer):**
- Durable writes: acked ⇒ persisted to object storage.
- Read-your-writes: the accepting node merges its memtable into queries.
- Linearizable namespace metadata operations (via manifest CAS).

**Multi-node:**
- Writes for a namespace are serialized through manifest CAS; concurrent
  writers are safe but may retry.
- Reads on nodes other than the writer see a manifest at most
  `manifest.poll_interval` (default 1s) stale, plus the indexer flush delay for
  WAL-tail data **unless** the read lands on the accepting node. Clients
  needing read-your-writes across nodes can pass
  `min_manifest_generation` (returned by every write) in queries; the server
  refreshes the manifest until it catches up or times out.
- There is **no cross-namespace transactionality** and no multi-document
  transaction within a namespace beyond atomic batch application.

**Recovery:** restart = read `CURRENT`. Partial WAL/segment uploads are
invisible (never referenced by a committed manifest). Crash during
indexing/compaction leaves orphan files that GC removes. The crash-recovery
test suite (`tests/recovery/`) exercises restart-after-write, restart
mid-flush, restart mid-compaction, and truncated/corrupted file handling.

---

## 9. Tradeoffs (read this before deploying)

- **Latency floor on cold queries.** First-touch queries pay object-storage
  round trips (tens to hundreds of ms per fetched file). Lagoon mitigates with
  coalesced range reads, warming, and pinning — it does not eliminate physics.
- **IVF, not graphs.** IVF-Flat was chosen because posting lists are
  block-fetchable from object storage and rebuildable deterministically.
  Graph indexes (HNSW) can give better recall/latency in RAM but are
  memory-resident and awkward to page from S3. Recall is tunable via `nprobe`
  (see measured recall@k in the benchmark guide).
- **Write visibility ≠ write durability across nodes.** Acked writes are
  durable immediately but may take up to the staleness window to be visible on
  other nodes.
- **Manifest CAS is a per-namespace write bottleneck.** Very high write
  concurrency to one namespace serializes on the pointer swap. Batch bigger;
  shard across namespaces if you need more.
- **Compaction costs reads + writes.** Object-storage request costs scale with
  churn. Tiered compaction amortizes this; tune `compaction.fanin` for your
  workload.
