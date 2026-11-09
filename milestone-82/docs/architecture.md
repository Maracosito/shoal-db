# Gannet Architecture

*Status: v1 design, Milestone 1. This document is the authoritative description of
Gannet's system architecture. The byte-level details live in
[storage-format.md](storage-format.md); the API surface lives in
[`api/openapi.yaml`](../api/openapi.yaml).*

---

## 1. Design principles

1. **Object storage is the database.** Every byte needed to reconstruct a namespace
   — documents, indexes, metadata, branch references — lives in an S3-compatible
   bucket as immutable (or append-only-replaced) objects. Local disk and memory are
   *caches*, never sources of truth. A node can be killed at any moment and a fresh
   node can serve the namespace from the bucket alone.

2. **Immutability everywhere except one object per namespace.** All data files
   (WAL chunks, segments, index files) are written once and never modified. The
   single mutable point is the namespace **manifest pointer**, advanced via
   object-storage conditional writes (compare-and-swap). This gives us atomic
   visibility, trivial caching (immutable objects cache forever), cheap branching
   (share immutable files), and simple recovery (the manifest is the only thing
   that can be "torn").

3. **Stateless compute, stateful cache.** Nodes keep an SSD + RAM cache keyed by
   content-addressed object names. Caches need no invalidation protocol because
   cached objects are immutable; only manifests are revalidated.

4. **Pay-per-query economics.** Cold namespaces cost only storage. The system is
   explicitly designed for fleets of thousands of mostly-idle namespaces.

5. **Honest consistency.** Single-writer-per-namespace with CAS fencing, durable
   batched WAL writes, read-your-writes on the writing node. Multi-node read
   scale-out is eventually consistent with a bounded, measurable staleness. We
   document exactly what you get (§7) instead of hand-waving.

---

## 2. System components

```
                        ┌──────────────────────────────────────────────┐
                        │              S3-compatible bucket            │
                        │  manifests · WAL chunks · segments · refs    │
                        └───────▲──────────────▲───────────────▲───────┘
                                │              │               │
              conditional PUT / │         GET (range)          │  PUT/GET
              GET manifest      │              │               │
                        ┌───────┴──────┐ ┌─────┴───────┐ ┌─────┴────────┐
   HTTP/JSON   ───────► │  gannetd     │ │  gannetd    │ │ gannet-worker│
   clients              │  (writer +   │ │  (read      │ │ (indexer +   │
                        │   reader)    │ │   replica)  │ │  compactor + │
                        │ SSD+RAM cache│ │ SSD+RAM     │ │  GC)         │
                        └──────────────┘ └─────────────┘ └──────────────┘
```

### 2.1 `gannetd` (API server)

Serves the HTTP API. Each namespace has, at any moment, at most one node acting as
its **writer** (lease recorded in the namespace manifest via CAS; see §5.4). Any
node may act as a **reader**. In the default single-node deployment one process is
both. `gannetd` performs:

- WAL appends (group-committed; §5),
- query execution over cached segments + in-memory WAL tail (§6),
- cache management (§8),
- inline "micro-indexing" of the WAL tail so fresh writes are immediately queryable.

### 2.2 `gannet-worker` (background plane)

Stateless background workers lease jobs through the same CAS mechanism:

- **Indexing**: turn accumulated WAL chunks into immutable segments (columnar doc
  store + vector index + inverted index + attribute indexes).
- **Compaction**: merge small segments into larger ones, drop tombstoned documents.
- **GC**: delete unreachable objects after a grace window, respecting branch
  reachability (§9.4).

Workers are optional for correctness — a namespace with only WAL chunks is fully
queryable, just slower — and the single-node `gannetd` embeds a worker by default.

### 2.3 Storage backends

A single trait (`ObjectStore` in `gannet-core`) abstracts:

| Backend | Use | Conditional write support |
|---------|-----|---------------------------|
| `file://` local filesystem | tests, embedded/dev | atomic rename + lock file emulation |
| `s3://` (AWS S3, and S3-compatible) | production | `If-Match`/`If-None-Match` (S3 has supported conditional writes since 2024) |
| `s3://` via MinIO | local dev / CI | native conditional writes |

Required operations: `put`, `put_if_not_exists`, `put_if_match(etag)`, `get`,
`get_range`, `head`, `list(prefix)`, `delete`. Backends without conditional-write
support are not supported for multi-node deployments (documented limitation).

---

## 3. Data model

```
organization ──► project ──► namespace ──► document
```

- **Organization / project**: administrative grouping; API keys are scoped to one
  or the other with a role (admin/writer/reader).
- **Namespace**: an isolated document space with its own manifest chain, WAL,
  segments, and configuration (vector dimensions, distance metric, FTS settings).
  Namespaces are the unit of branching, copying, warming, pinning, and export.
- **Document**: `{ id, vector?, sparse_vector?, text fields, attributes }`.
  - `id`: UTF-8 string ≤ 256 bytes (or u64 presented as string).
  - `vector`: optional dense `float32[dim]`; `dim` fixed per namespace at first write.
  - `sparse_vector`: optional `{ indices: u32[], values: f32[] }`.
  - Text fields: string attributes flagged for full-text indexing.
  - Attributes: JSON scalars and arrays of scalars (string, i64, f64, bool,
    arrays thereof). Nested objects are stored but not filterable in v1.

---

## 4. Object storage layout (summary)

The full normative layout is in [storage-format.md](storage-format.md). Shape:

```
gannet/v1/
  orgs/{org}/projects/{proj}/namespaces/{ns_id}/
    NSROOT                            ← tiny mutable pointer: current manifest id (CAS'd)
    manifests/{generation:020}.json   ← immutable manifest snapshots
    wal/{seq:020}-{ulid}.wal          ← immutable WAL chunks
    segments/{ulid}/
        seg.meta.json                 ← segment manifest (immutable)
        docs.col                      ← columnar document store
        vec.ivf                       ← IVF vector index (centroids + posting lists)
        fts.idx                       ← inverted index + BM25 stats
        attr.idx                      ← attribute/zone indexes
        dels.bmp                      ← tombstone bitmap sidecars (per overlaying op)
    branches.json                     ← child-branch registry (CAS'd, for GC safety)
  orgs/{org}/projects/{proj}/_catalog/…   ← project/namespace registry, API key hashes
```

Key property: everything under `manifests/`, `wal/`, `segments/` is immutable.
Only `NSROOT` and `branches.json` (and catalog objects) are replaced, always via
conditional writes.

**The manifest** is the namespace's entire logical state: format version, namespace
config, list of live segments (with per-segment doc counts, tombstone sidecars, and
attribute statistics), list of un-segmented WAL chunks with their sequence range,
the next WAL sequence number, branch parentage (`parent_ns`, `fork_generation`), and
the writer lease. Advancing `NSROOT` from manifest *N* to *N+1* via CAS is the
single atomic commit point in the whole system.

---

## 5. Write path

### 5.1 Flow of an upsert

```
client POST /v1/.../documents:upsert
  └─► gannetd validates batch (ids, vector dims, attribute types)
       └─► batch enters the namespace's group-commit queue
            └─► flusher drains queue (≤ max_batch_bytes or ≤ flush_interval, default 50ms)
                 ├─► serialize WAL chunk (framed records, CRC32C per frame, zstd)
                 ├─► PUT wal/{seq}-{ulid}.wal           (immutable, if-not-exists)
                 ├─► build manifest N+1 (append WAL ref, bump next_seq)
                 ├─► CAS NSROOT: N → N+1                ← durability point
                 ├─► apply chunk to in-memory WAL tail (memtable) for serving
                 └─► respond 200 to all batches in the flush
```

A write is acknowledged **only after** both the WAL chunk PUT and the NSROOT CAS
succeed. Latency floor is therefore two sequential object-storage round trips
(one PUT + one CAS); group commit amortizes this across concurrent writers, so
throughput scales with batch size, not request count.

*Optimization (v1):* the WAL PUT and a speculative manifest PUT are pipelined; only
the final CAS is serial. On CAS conflict (another writer won the lease), the write
is failed with `409 writer_fenced` and the chunk becomes garbage (GC collects it —
unreferenced WAL chunks are harmless).

### 5.2 Upsert semantics

- **Upsert** replaces a document wholly by `id`. Last-writer-wins is defined by WAL
  sequence order (which is total per namespace, because manifest CAS serializes it).
- **Patch** records a partial-update WAL record `{id, set: {...}, unset: [...]}`;
  it is resolved against the latest version at read/index time. Patching a
  nonexistent document is an error reported in the per-row results.
- **Delete by id** records a tombstone WAL record.
- **Delete by filter** is evaluated *at write time* against the current view, and
  the matching ids are recorded as explicit tombstones (so the operation is
  deterministic and replayable, rather than re-evaluating the filter on recovery).
  The response reports the count. For very large matches the server paginates
  internally across multiple WAL chunks within one logical operation.

### 5.3 Idempotency and conditional writes

- Every write request may carry an `idempotency_key` (client-chosen string). The
  flusher embeds `(idempotency_key, request_hash)` in the WAL chunk header and the
  manifest keeps a rolling window (default: 24 h or 64 K keys) of seen keys. Replays
  inside the window return the original result without re-applying.
- **Conditional writes** per document: `if_version: <u64>` (CAS on the document's
  monotonically increasing version, which is its last WAL sequence number) and
  `if_absent: true` (insert-only). Failures are reported per-row with the current
  version, so clients can do optimistic concurrency.

### 5.4 Single-writer fencing

The manifest contains `writer_lease: {node_id, epoch, expires_at}`. A node acquires
or renews the lease via the same manifest CAS that commits writes; a node whose
epoch is stale can never commit (its CAS will fail) — fencing is therefore *built
into the commit*, not a separate lock service. Lease TTL default 15 s; a crashed
writer's namespace is writable again after TTL expiry by any node.

### 5.5 Background indexing & compaction

- When un-segmented WAL bytes exceed a threshold (default 8 MiB) or age (default
  60 s), an indexing job converts WAL chunks `[a..b]` into a new immutable segment
  and commits a manifest that swaps `wal[a..b]` for the segment — again via NSROOT
  CAS, so indexing never races with writes incorrectly (it just retries on a fresh
  manifest, re-using the already-built segment if its WAL range is still a prefix).
- Compaction is leveled-by-size: segments are merged when a level has more than
  `fanout` (default 8) segments, dropping tombstoned and superseded versions —
  except where a child branch still references the inputs (then inputs are retained
  and only the manifest changes; GC handles the rest, §9.4).
- Both jobs are crash-safe: they write all outputs as new immutable objects first,
  CAS the manifest last. A crash before CAS leaves only unreachable garbage.

### 5.6 Recovery

Recovery is *opening the namespace*: `GET NSROOT → GET manifest → done`. The
manifest is always internally consistent because it is committed atomically.
WAL chunks listed in the manifest are replayed into the memtable on first access
(frames are CRC-checked; a torn final frame in a chunk is impossible by
construction since chunks are immutable single PUTs — a failed PUT simply never
appears in any manifest). There is no fsck. Crash-recovery tests (Milestone 4)
assert: restart after writes, restart mid-indexing, restart mid-compaction, and
tolerance of junk/partial objects not referenced by any manifest.

---

## 6. Query path

### 6.1 Execution model

A query against namespace view = **manifest generation G** executes over:

```
results = merge( memtable/WAL-tail view , segment₁ , … , segmentₙ )
          with per-segment tombstone bitmaps + version shadowing applied
```

Steps:

1. **Snapshot.** Resolve `NSROOT` → manifest G (readers may serve a cached manifest
   within `max_staleness`; the writing node always uses its latest, giving
   read-your-writes — §7).
2. **Plan.** The planner (§6.5) chooses per-segment strategies.
3. **Hydrate.** Fetch needed *parts* of segment files via cached range reads:
   IVF centroid table, selected posting lists, needed columns. Cache hits skip
   object storage entirely.
4. **Execute** per segment + memtable in parallel; apply filters; score.
5. **Merge** top-k across sources, resolving duplicate ids by highest WAL version.
6. **Project** requested attributes (`include_attributes`), return scores and
   optional vectors.

### 6.2 Vector search

- **Exact kNN**: scan vector column (SIMD f32 kernels for cosine/dot/L2). Always
  available; the baseline for recall measurement.
- **ANN — IVF**: each segment's `vec.ivf` holds k-means centroids (k ≈ √n, tuned)
  and per-centroid posting lists of `(doc_ordinal, vector)` laid out contiguously
  so a probe is one range read per centroid. Query probes `nprobe` nearest
  centroids (default 16, request-overridable as `ann.nprobe`); recall target
  ≥ 0.95@10 on standard datasets, verified in the benchmark suite. IVF was chosen
  over HNSW because probes map to a handful of large sequential range reads —
  ideal for object-storage-backed partial hydration — whereas graph traversal
  requires the whole graph resident.
- Distances: cosine (vectors normalized at index time; raw stored optionally),
  dot product, Euclidean. Metric fixed per namespace.
- **Filtered ANN**: filters are evaluated to a candidate bitmap first when
  selective (see planner); IVF scanning skips non-matching ordinals. If the filter
  passes < `exact_threshold` (default 5 000) candidates, the planner switches to
  exact scoring over just those candidates — better recall *and* faster.

### 6.3 Full-text search

Per-segment inverted index: Unicode-aware tokenizer (lowercase, configurable
stemming off by default), term dictionary (FST), postings with positions optional,
block-encoded doc ids + term frequencies. Scoring is BM25 (`k1=1.2`, `b=0.75`,
per-namespace configurable) with per-field boosts; multi-field queries use BM25F-style
weighted field combination. Corpus statistics (doc count, average field length) are
kept per segment and merged at query time for globally consistent IDF.

### 6.4 Hybrid & sparse search

- A query may contain `rank_by` clauses for vector and text simultaneously, fused by:
  - **`rrf`** (reciprocal rank fusion, k=60 default), or
  - **`weighted`** (min-max-normalized score blend with user weights).
- **Multi-query**: `POST …/query` accepts `queries: [...]` (≤ 16) executed against
  one snapshot, sharing hydration work; responses are positionally matched.
- **Sparse vectors** are served by the same inverted-index machinery (indices =
  terms, values = weights, dot-product scoring), making SPLADE-style retrieval
  natural.

### 6.5 Query planner

Inputs: query shape, manifest statistics (per-segment doc counts, attribute
min/max/ndv, tombstone density), filter selectivity estimates, cache state.

Decision tree (v1, deliberately simple and fully logged in `debug.plan`):

```
for each segment:
  1. prune: if attribute zone-maps prove filter unsatisfiable → skip segment
  2. if filter present:
       est = selectivity(filter)               # from attr.idx statistics
       if est * seg.docs < exact_threshold:    # very selective
           plan = FILTER_FIRST → exact score candidates
       else:
           plan = bitmap filter + (IVF probe | FTS postings) with skip-checks
  3. if vector query and seg.docs < brute_threshold (default 10 000):
       plan = EXACT_KNN  (small segments aren't worth probing)
  4. else vector → IVF, text → FTS postings, hybrid → both + fuse
memtable: always exact (it is small by construction)
```

`top_k ≤ 1 000`; pagination is offset-free (cursor over score+id) in a later
milestone.

---

## 7. Consistency model

### 7.1 Single-node (default deployment)

- **Durability**: a `200` write response means the WAL chunk is persisted in object
  storage *and* referenced by a committed manifest. Loss requires losing the bucket.
- **Atomicity**: a write batch is wholly visible or not at all (one WAL chunk, one
  manifest commit).
- **Read-your-writes**: the node serves queries from the manifest+memtable that
  include every write it has acknowledged. Strictly monotonic reads.
- **Ordering**: total order per namespace = WAL sequence order.

### 7.2 Multi-node (writer + read replicas)

- One writer per namespace (CAS-fenced lease). Writes have the same guarantees as
  single-node.
- Read replicas poll/revalidate `NSROOT` and serve a **possibly stale snapshot**,
  bounded by `replica.max_staleness` (default 5 s — one cheap conditional GET per
  namespace per interval). Within one replica, reads are monotonic (generation
  never goes backward).
- **Read-your-writes across nodes is not guaranteed** in v1. Mitigations available
  to clients: (a) route reads for write-heavy namespaces to the writer (the
  response header `Gannet-Generation: <G>` plus request param
  `min_generation=<G>` lets a replica wait/refresh until it reaches the client's
  write generation — bounded read-your-writes "tokens"); (b) accept staleness.
- There is no mode where two nodes accept writes to the same namespace; CAS makes
  split-brain commits impossible (a fenced writer's CAS fails).

### 7.3 What can go wrong (explicitly)

| Scenario | Outcome |
|---|---|
| Crash after WAL PUT, before CAS | Write not acknowledged, not visible; chunk is unreachable garbage; client retries (idempotency key dedupes). |
| Crash after CAS, before response | Write durable + visible; client retry is deduped by idempotency key. |
| Two writers race | Exactly one CAS wins; loser returns `409`, its chunk is garbage. |
| Replica serves during writer failover | Stale reads up to `max_staleness`; never torn reads. |
| Object store returns stale `NSROOT` on GET | Only possible on non-read-after-write stores; S3/MinIO/GCS are read-after-write for single keys — listed as a backend requirement. |

---

## 8. Cache hierarchy

```
RAM  (moka LRU)  : manifests, centroid tables, term dictionaries, hot postings,
                   attribute indexes, memtables          — default 25% of RAM
SSD  (foyer-style hybrid cache, content-addressed files) : segment file ranges
                   keyed by (object_key, range)          — default configurable GiB
S3                : everything, always
```

- Immutable objects → cache entries never invalidate; eviction is LRU with
  size-aware admission (don't evict a hot centroid table for a cold doc column).
- **Warm endpoint** (`POST …:warm`): hydrates a namespace's manifest + selected
  tiers (`metadata` | `indexes` | `full`) into SSD/RAM, optionally blocking until
  done; returns bytes hydrated.
- **Pinning** (`POST …:pin`): pinned namespaces are exempt from eviction up to a
  configured pin budget; re-hydrated automatically on node restart.
- Metrics distinguish `cache_hit{tier="ram|ssd"}` vs `object_store_read`, and the
  benchmark suite reports cold/warm latency split per query class.

---

## 9. Namespace branching (copy-on-write)

### 9.1 Mechanism

`POST …/namespaces/{src}:branch {name}` performs:

1. Read source manifest at generation G.
2. Write a new namespace whose first manifest references **the same immutable
   segment and WAL objects** (by absolute object key) and records
   `parent: {ns: src, generation: G}`.
3. CAS-append the child into the source's `branches.json` registry (for GC).

Cost: O(manifest size), milliseconds, zero data copied.

### 9.2 Isolation

- Writes to the branch produce new WAL chunks/segments under the **branch's own
  prefix**; the source's objects are never modified (they are immutable).
- Writes to the source likewise create new objects; the branch pinned generation G
  and never re-reads the source's NSROOT. Mutual isolation is structural, not
  enforced by checks.
- Branches of branches work identically (multi-level); each manifest may reference
  objects across any ancestor prefixes.

### 9.3 Compaction across branches

Compaction in a branch may merge a mix of inherited and own segments; outputs go
under the branch's prefix. Inherited objects are never deleted by the branch.

### 9.4 Deletion & GC

Deleting a namespace marks its NSROOT with a deletion record (tombstone manifest).
GC computes reachability: an object is deletable only when **no live manifest of
the namespace or any registered descendant branch references it** and the grace
window has elapsed. `branches.json` registries make descendants discoverable
without listing the world. Tests (Milestone 5) cover: branch isolation both
directions, multi-level branches, deleting a parent with live children (data
retained until children die), and deleting branches without touching shared data.

### 9.5 Copy vs branch

`:copy` is a physical copy (server-side object copy of all referenced files into
the new prefix) — slower, but yields a fully independent namespace with no GC
entanglement. `:branch` is the COW path. Both are exposed; the API doc explains
when to use which.

---

## 10. Observability, security, operations

- **Logs**: structured (`tracing` + JSON), request-scoped fields
  (org/project/ns/request_id), secrets never logged (API keys are accepted only in
  headers and stored only as salted SHA-256 hashes).
- **Metrics** (Prometheus `/metrics`): query latency histograms per class
  (vector/fts/hybrid, cold/warm), cache hit/miss per tier, object-store ops + bytes
  + latency, WAL bytes pending indexing, indexing lag seconds, segment counts per
  level, compaction queue depth, write group-commit batch sizes.
- **Tracing**: OpenTelemetry spans on the request path and storage operations
  (OTLP exporter, off by default).
- **Audit log**: append-only JSONL stream (namespace lifecycle, branch/copy/delete,
  write summaries, key usage) to object storage under `_audit/`.
- **Quotas/rate limits** (optional config): per-key RPS, per-namespace document
  and byte caps, request-size caps.
- **Roles**: `admin` (everything incl. keys), `writer` (data writes + warm),
  `reader` (query/export). Keys scoped to org or project.
- **Encryption at rest**: rely on bucket-level SSE (SSE-S3/SSE-KMS); documented in
  the deployment guide. Gannet adds checksums, not crypto, in v1.

---

## 11. Tradeoff analysis (explicit)

| Decision | Chosen | Rejected alternative | Why |
|---|---|---|---|
| Source of truth | Object storage only | Replicated local disks (Raft) | 10–20× cheaper at rest; stateless compute; we accept higher write latency and a single-writer model. |
| Commit protocol | Manifest CAS (single mutable pointer) | Per-object listing as truth ("just list the WAL prefix") | Listing is slow, costly, and eventually consistent on some stores; CAS gives an atomic, totally ordered commit point and free fencing. |
| WAL granularity | One immutable chunk per group-commit | Append to one growing object | S3 objects are immutable; multipart "appends" are fragile and unportable. Many small chunks are merged away by indexing anyway. |
| Vector index | IVF (centroid posting lists) | HNSW | IVF probes = few large sequential range reads, partial hydration works; HNSW needs the whole graph warm and random access. We accept somewhat lower recall/QPS at equal memory when fully warm. |
| Visibility of fresh writes | Memtable over WAL tail | Force-index before ack | Sub-100 ms write visibility without paying indexing on the write path; we accept slower (exact) search over the small tail. |
| Multi-node consistency | Single writer + stale replicas + generation tokens | Consensus replication | Massive complexity savings; matches the workload (read-heavy, many namespaces); we accept no cross-node linearizable reads in v1. |
| Delete-by-filter | Resolve to ids at write time | Store filter, re-evaluate lazily | Deterministic replay & branching semantics; we accept extra WAL bytes for huge deletes. |
| GC | Delayed reachability mark-and-sweep | Eager refcounting | Refcount updates would need their own transactional store; delayed sweep is simple and branch-safe; we accept temporary storage overhead. |
| Formats | Own minimal binary formats + JSON manifests | Parquet/Lucene/Tantivy files | Tight control over range-read layout and versioning; we accept building/maintaining format code. (Tantivy *ideas* inform the FTS design.) |

## 12. Failure-mode walkthroughs

1. **Node loss mid-write** → client times out, retries with same idempotency key →
   either deduped (commit had happened) or re-applied cleanly. No torn state
   possible (§5.6).
2. **Worker dies mid-compaction** → outputs are unreachable garbage; lease expires;
   another worker redoes the job. Cost: wasted work only.
3. **Bucket throttling (503s)** → exponential backoff with jitter at the
   `ObjectStore` layer; writes back-pressure via the group-commit queue (bounded;
   overflow → `429`).
4. **Cache disk full** → eviction; if pinned set exceeds budget, pin requests fail
   loudly rather than silently degrading.
5. **Clock skew and leases** → leases use object-store-observed time ordering plus
   epochs; correctness never depends on clocks (CAS epochs fence), only liveness
   (a very skewed node may fail to acquire leases promptly).

## 13. Glossary

| Term | Meaning |
|---|---|
| **Manifest** | Immutable JSON snapshot of a namespace's complete logical state at one generation. |
| **NSROOT** | The one mutable object per namespace; holds the current manifest generation; advanced by CAS. |
| **WAL chunk** | Immutable object containing one group-commit's framed write records. |
| **Segment** | Immutable indexed unit: doc columns + vector/FTS/attribute indexes. |
| **Memtable / WAL tail** | In-RAM replay of WAL chunks not yet folded into segments. |
| **Generation** | Monotonic manifest counter; doubles as a consistency token. |
| **Branch** | Namespace whose initial manifest references another namespace's immutable objects at a fixed generation. |
