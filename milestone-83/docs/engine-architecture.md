# Reef Storage Engine Architecture

This document describes the durable write path and storage engine delivered in
milestone 2: the write-ahead log (WAL), segments, manifests, the background
indexer, compaction, recovery, and the consistency model. It is the
authoritative reference for the on-store format. Higher layers (query
execution, caching, branching) build on the invariants defined here.

> **Scope note.** Reef is an original, clean-room project. Object storage as
> the source of truth with stateless compute is a publicly known architectural
> pattern; everything below is Reef's own design.

## 1. Design goals

1. **Object storage is the only durable state.** A compute node can be
   destroyed at any moment and a fresh node must be able to serve the
   namespace from the bucket alone. Local disk and memory are caches, never
   sources of truth.
2. **Append-only, immutable objects.** No object is ever modified in place.
   Visibility changes happen only through *manifest publication*.
3. **Crash-safety by construction.** Every multi-step operation is ordered so
   that a crash at any point leaves either (a) the old state, or (b) the new
   state, or (c) the old state plus unreachable orphan objects that GC removes
   later. There is no crash point that produces a state requiring manual
   intervention to serve reads.
4. **Minimal backend requirements.** The engine needs only whole-object
   atomic PUT, conditional create (`If-None-Match: *` / put-if-absent),
   GET (with ranges), DELETE, and prefix listing. No rename, no append, no
   conditional overwrite. This keeps every S3-compatible service viable.

## 2. On-store layout

All objects for a namespace live under a single prefix:

```
namespaces/<ns>/
  manifest/
    <generation>.manifest.json     # versioned manifests, generation = zero-padded u64
  wal/
    <lsn>.wal                      # one object per committed write batch
  segments/
    <segment-id>.seg               # immutable queryable segments
  quarantine/
    ...                            # objects moved aside by `repair`
```

Key properties of the naming scheme:

* WAL objects are named by their **log sequence number (LSN)**, zero-padded so
  lexicographic listing order equals log order.
* Manifests are named by **generation**, also zero-padded, so the latest
  manifest is the last key under `manifest/`.
* Segment ids are unique (ULID-style), so a re-run of a crashed indexing or
  compaction job can never collide with a previously published segment.

## 3. Write path

### 3.1 Commit protocol

A write batch (upserts, patches, deletes — row- or column-oriented at the API
boundary, normalized to row operations internally) commits as follows:

1. The writer validates the batch (id syntax, vector dimensions against the
   namespace schema, value types) and evaluates any **conditional write**
   predicates against current visible state under the writer lock.
2. The batch is checked against the **idempotency window** (§3.4). A replayed
   key returns the original receipt without re-writing.
3. The batch is serialized into a WAL object and PUT at `wal/<next-lsn>.wal`
   using **put-if-absent**. Success of this conditional PUT *is the commit
   point*: the data is durable and the LSN is allocated in one atomic step.
   If the conditional PUT loses (another writer holds the namespace), the
   writer re-syncs from the manifest and either retries or surfaces a fencing
   error.
4. The batch is applied to the in-memory **memtable** overlay so the writing
   node immediately serves read-your-writes.

A WAL object is fully self-describing:

```
[ magic "RFWAL1\0\0" | format version u32 ]
[ header: lsn, batch op count, created_at, idempotency key?, condition record? ]
[ record 0: u32 length | u32 crc32c | payload ]
[ record 1: ... ]
[ footer: total record count | crc32c of header+records ]
```

Per-record CRCs detect partial corruption; the footer detects truncation. A
WAL object that fails any check is treated as **never committed** (see §7.3 —
because the PUT is whole-object atomic, a corrupt WAL object can only result
from storage-layer damage, not from a writer crash).

### 3.2 Batched upserts

Clients are encouraged to batch. One WAL object per batch amortizes the PUT
round-trip; the engine additionally coalesces concurrent small batches from
the same process into a single WAL object when they arrive within the group
window (default 5 ms), preserving per-batch receipts.

### 3.3 Patches and deletes

* **Patch** records carry only the changed attributes; the indexer resolves
  them against the newest prior version of the document (memtable → newer
  WAL → segments) when folding.
* **Delete** records become tombstones. Tombstones shadow older versions at
  read time and are physically dropped during compaction once no older
  segment can contain the id.

### 3.4 Idempotent writes

A batch may carry an `idempotency_key`. The manifest stores a bounded,
sliding **dedup window** mapping recent keys → (LSN, receipt digest). Replays
inside the window are acknowledged without new WAL objects. The window is
bounded by count and age (configurable; default 10 000 keys / 24 h) and is
advanced by the indexer when it publishes manifests — so deduplication
survives restarts, because the window lives in the manifest, not in memory.

### 3.5 Conditional writes

Supported predicates: *create-only* (fail if the id exists), *must-exist*
(fail if absent), and *attribute equality* guards. Predicates are evaluated
under the single-writer lock against the visible state at commit time, and a
copy of the predicate is recorded in the WAL header so recovery replays reach
the same accept/reject decision deterministically.

## 4. Manifest

The manifest is the **root of visibility**. Generation `g` is published by
`put_if_absent("manifest/<g+1>.manifest.json")` — a compare-and-swap on plain
object storage. A manifest contains:

* format version and namespace schema fingerprint
* the **segment list**: id, level, row count, tombstone count, key/LSN range,
  size, and per-file checksum for every live segment
* `wal_floor`: the first LSN **not yet folded** into segments
* `last_applied_lsn`: the newest LSN reflected in the segment set
* the idempotency dedup window (§3.4)
* a writer **fencing token** (epoch) — a writer that fails a manifest CAS
  knows it has been superseded and must re-open the namespace
* GC bookkeeping: tombstoned object keys awaiting grace-period deletion

Manifests are small (KBs) and cached aggressively. Old manifest generations
are retained for a configurable horizon, which gives time-travel-style debug
reads and is the anchor for namespace branching in a later milestone.

## 5. Read path inside the engine

A read resolves a document id through three layers, newest first:

1. **Memtable** — writes applied on this node since the last fold.
2. **Unfolded WAL range** — `[wal_floor, head]`, replayed into the memtable at
   open time (so after restart, layer 1 already contains layer 2).
3. **Segments** — consulted newest-to-oldest by LSN range; the first hit
   (value or tombstone) wins.

Segments carry bloom filters over ids so point reads usually touch one
segment. Range/scan reads merge iterators across layers with the same
newest-wins rule.

## 6. Background indexer

The indexer folds WAL entries into segments. One pass:

1. Load the latest manifest; list `wal/` keys in `[wal_floor, head]`.
2. Stream and validate those WAL objects, building sorted row blocks,
   resolving patches, and materializing tombstones.
3. Write one or more **segment objects** with fresh unique ids:

   ```
   [ magic "RFSEG1\0\0" | format version ]
   [ row blocks: sorted by id, each block length-prefixed + crc32c ]
   [ column blocks: vectors / attribute columns, each crc32c'd ]
   [ id bloom filter ]
   [ block index: id ranges → block offsets ]
   [ footer: offsets of all sections, stats (min/max id, lsn range,
     row & tombstone counts), footer crc32c, magic again ]
   ```

   The footer-last layout means a truncated upload is detectable from the
   tail alone, and a single ranged GET of the footer is enough to plan reads.
4. Publish a new manifest (CAS) that adds the segments and advances
   `wal_floor`. **Only this step changes visibility.**
5. Move folded WAL keys into the GC pending list (deleted later, after the
   grace period).

If the indexer crashes after step 3 but before step 4, the uploaded segments
are unreachable orphans: no manifest references them, `wal_floor` is
unchanged, and the next pass re-folds the same WAL range under new segment
ids. Correctness is unaffected; GC reclaims the orphans.

## 7. Compaction

### 7.1 Strategy

Size-tiered: the planner picks the smallest N segments in a level whose
combined size crosses the level threshold, merges them (newest-wins, dropping
shadowed versions and expirable tombstones), writes the merged segment(s)
under fresh ids, and publishes a manifest that atomically swaps inputs for
outputs. Inputs go onto the GC pending list with a timestamp.

### 7.2 Atomicity

Compaction has exactly one visibility step — the manifest CAS — so a crash at
any other point leaves the previous manifest fully serving and at worst some
orphan objects. Readers holding the old manifest keep working because input
segments are not physically deleted until the GC grace period (default
15 minutes) has passed *and* no retained manifest generation references them.

### 7.3 Corruption and incompleteness handling

| Condition | Detection | Handling |
|---|---|---|
| Truncated segment upload (crash mid-PUT) | impossible to observe — object stores expose whole-object PUTs atomically; a partial upload never becomes visible | n/a |
| Segment bytes damaged at rest | footer/block crc32c mismatch, size mismatch vs manifest | `verify` flags it; `repair` quarantines the segment and `rebuild` reconstructs from WAL if still retained, else from the most recent healthy manifest generation |
| WAL object damaged at rest | record/footer crc mismatch | treated as never-committed if it is the head; `repair` quarantines and reports the affected LSN range otherwise |
| Manifest damaged | JSON/schema/checksum failure | open falls back to the previous generation; `repair --apply` republishes it as the head |
| Orphan segments/WAL (crash between upload and publish) | referenced by no retained manifest | `gc` deletes after grace period |

## 8. Recovery

Opening a namespace:

1. List `manifest/`, take the highest generation that validates (falling back
   generation-by-generation on damage, which is logged loudly).
2. List `wal/` from `wal_floor`; validate and replay each object into the
   memtable in LSN order. The head WAL object, if invalid, is discarded as
   uncommitted; an invalid object *followed by valid ones* is a hard error
   surfaced to `repair` (it indicates at-rest damage, not a crash).
3. Resume background indexing/compaction from exactly the manifest state.

Crash matrix (all covered by tests):

| Crash point | State after restart |
|---|---|
| After WAL PUT, before memtable apply | Batch is durable; replay at open restores it. Read-your-writes holds. |
| Mid-indexing, after segment upload, before manifest CAS | Old manifest serves; WAL re-folded under new ids; orphans GC'd. |
| Mid-compaction, before manifest CAS | Inputs still live; merge re-run; orphans GC'd. |
| After manifest CAS, before WAL/old-segment cleanup | New state serves; cleanup completes via GC pending list. |
| During GC | Pending list is re-derived from manifests; deletes are idempotent. |

## 9. Consistency model

* **Durability.** A write is durable when its WAL object PUT succeeds. The
  client receipt is returned only after that.
* **Single-writer per namespace.** Exactly one engine instance may accept
  writes for a namespace at a time, enforced by LSN put-if-absent and the
  manifest fencing epoch: a superseded writer deterministically fails its next
  WAL or manifest CAS and steps down. Split-brain cannot publish.
* **Read-your-writes** on the writing node, immediately (memtable overlay).
* **Other readers** (multi-node mode) are *eventually consistent*: they see a
  write once they observe a manifest at or past it, or by tailing the WAL.
  Staleness is bounded by the reader's manifest/WAL poll interval
  (configurable; this tradeoff buys stateless, horizontally scalable readers).
* **No cross-namespace transactions.** A WAL batch is atomic within its
  namespace: all of its operations become visible together.

## 10. Garbage collection

GC is two-phase. Phase 1 (performed by indexer/compaction): superseded object
keys are added to the manifest's pending list with timestamps. Phase 2
(`reef gc`): keys are deleted only when (a) the grace period has elapsed and
(b) no retained manifest generation references them. Orphans never listed in
any manifest are detected by prefix-listing diff and given the same grace
period keyed off their `LastModified` time. Deletes are idempotent, so a
crash mid-GC is harmless.

## 11. Operational commands

* `reef verify [--deep]` — manifest chain, WAL framing, segment footers;
  `--deep` re-checksums every block. Exit code 2 on problems.
* `reef rebuild` — reconstructs all derived state from the bucket alone,
  proving disaster recovery works.
* `reef repair [--apply]` — quarantines damaged objects, rolls the manifest
  head back past invalid generations; dry-run by default.
* `reef gc [--apply]` — phase-2 deletion; dry-run by default.
* `reef index` / `reef compact` — run one background pass in the foreground
  (useful in tests and cron-style deployments without a resident worker).

## 12. Tradeoffs and future work

* **PUT-per-commit latency.** Commit latency is one object-store PUT
  (~10–80 ms on S3, ~1–5 ms on MinIO/SSD). Group commit amortizes throughput;
  latency-sensitive deployments should batch. We deliberately did not add a
  secondary low-latency log (e.g., a replicated disk log) in v1 to preserve
  the "bucket is everything" property.
* **Single writer.** Per-namespace write throughput is bounded by one writer.
  Namespaces are the sharding unit; this matches the intended workload
  (many isolated tenant namespaces).
* **List-based discovery.** Recovery and GC rely on prefix listing, which is
  rate-limited on some providers; the zero-padded naming keeps listings
  ranged and cheap.
* Planned on this foundation: filter/vector/text index blocks inside segments
  (milestone 3–4), copy-on-write branching anchored on manifest generations
  (milestone 6), and a distributed cache tier (milestone 5).
