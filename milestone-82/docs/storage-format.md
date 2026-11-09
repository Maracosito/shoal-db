# Gannet Storage Format Specification

*Normative specification, format version **1**. Milestone 1.*

All multi-byte integers are **little-endian** unless stated. All checksums are
**CRC-32C (Castagnoli)**. All text is UTF-8. "MUST/SHOULD/MAY" per RFC 2119.

---

## 1. Versioning and compatibility rules

1. Every persistent artifact carries a **format version** integer:
   - JSON artifacts: top-level `"format_version": <u32>`.
   - Binary artifacts: a fixed 8-byte magic followed by `u16 version` in the header.
2. **Readers** MUST refuse artifacts with a *major* format version greater than
   they support, with error `format_too_new`. The current major version is `1`.
3. **Forward-compatible JSON**: readers MUST ignore unknown JSON fields. Writers
   MUST NOT change the meaning of existing fields; new semantics require new
   fields or a version bump.
4. **Binary evolution**: each binary header contains `u16 version` and
   `u32 header_len`. Readers MUST honor `header_len` when skipping to the body so
   headers can grow additively within a major version.
5. **Manifest `min_reader_version`**: a manifest declares the minimum reader
   version required for the *set* of artifacts it references; nodes compare this
   single field at open time instead of probing every file.
6. **Per-feature flags**: optional encodings (e.g., int8 quantized vectors) set
   named flags in `seg.meta.json` (`"features": ["vec_i8", ...]`). Readers MUST
   reject segments with unknown *required* features (`features_required`) and MAY
   ignore unknown *optional* features (`features_optional`).
7. A format version bump to `2` is permitted only with a documented migration tool
   that rewrites or wraps version-1 artifacts; in-place reinterpretation is
   forbidden.

Magic numbers (8 bytes, ASCII):

| Artifact | Magic |
|---|---|
| WAL chunk | `GNTWAL\x00\x01` |
| Document column store | `GNTDOC\x00\x01` |
| IVF vector index | `GNTIVF\x00\x01` |
| FTS index | `GNTFTS\x00\x01` |
| Attribute index | `GNTATR\x00\x01` |
| Tombstone bitmap | `GNTDEL\x00\x01` |

---

## 2. Key layout

Root prefix (configurable, default `gannet/v1/`):

```
gannet/v1/
  catalog/
    orgs/{org_id}.json                      org record (CAS-replaced)
    keys/{key_id}.json                      API key record: salted hash, role, scope
  orgs/{org}/projects/{proj}/
    project.json                            project record (CAS-replaced)
    namespaces/{ns_id}/
      NSROOT                                see §3
      manifests/{generation:020d}.json      see §4 (immutable)
      wal/{first_seq:020d}-{ulid}.wal       see §5 (immutable)
      segments/{segment_ulid}/
        seg.meta.json                       see §6.1 (immutable)
        docs.col                            see §6.2 (immutable)
        vec.ivf                             see §6.3 (immutable, optional)
        fts.idx                             see §6.4 (immutable, optional)
        attr.idx                            see §6.5 (immutable, optional)
      dels/{ulid}.bmp                       see §7 (immutable)
      branches.json                         see §8 (CAS-replaced)
    _audit/{date}/{ulid}.jsonl              append-only audit batches (immutable)
```

Rules:

- `ns_id` is a ULID assigned at creation; the human name → ns_id mapping lives in
  `project.json` (so renames don't move objects).
- `{generation:020d}` and `{first_seq:020d}` are zero-padded for lexicographic =
  numeric ordering under `list`.
- Objects under `manifests/`, `wal/`, `segments/`, `dels/`, `_audit/` are
  **immutable**: written once with `put_if_not_exists`, never overwritten.
- Branch manifests MAY reference objects under *other* namespaces' prefixes by
  absolute key (see §4 `external: true`).

---

## 3. `NSROOT` — the namespace root pointer

Small JSON object, replaced only via conditional write (CAS on ETag, or
`put_if_not_exists` for generation 0):

```json
{
  "format_version": 1,
  "ns_id": "01J8ZB4N2T9CWX5H6M7QK3R8VD",
  "generation": 42,
  "manifest_key": "…/manifests/00000000000000000042.json",
  "state": "active",                  // active | deleting | deleted
  "updated_at": "2025-06-01T12:00:00Z"
}
```

- The committer MUST have already PUT the manifest object before CAS-ing NSROOT.
- `state: deleting` is the namespace tombstone consumed by GC; `deleted` remains
  as a marker for the grace window, then the whole prefix may be removed.
- Readers open a namespace by `GET NSROOT` then `GET manifest_key`. Backends MUST
  provide read-after-write on single-key GET (S3, GCS, MinIO, Azure all do).

## 4. Manifest

Immutable JSON, one per generation:

```json
{
  "format_version": 1,
  "min_reader_version": 1,
  "ns_id": "01J8ZB4N…",
  "generation": 42,
  "created_at": "2025-06-01T12:00:00Z",
  "config": {
    "vector": { "dim": 768, "metric": "cosine" },         // null until first vector
    "fts": { "language": "en", "stemming": false,
              "fields": { "title": {"boost": 2.0}, "body": {"boost": 1.0} } },
    "attribute_types": { "lang": "string", "stars": "i64", "tags": "string[]" }
  },
  "writer_lease": { "node_id": "n-abc", "epoch": 7,
                    "expires_at": "2025-06-01T12:00:15Z" },
  "next_wal_seq": 1019,
  "wal": [
    { "key": "…/wal/00000000000000001003-01J8ZC….wal",
      "first_seq": 1003, "last_seq": 1010, "bytes": 524288, "crc32c": 305419896 },
    { "key": "…/wal/00000000000000001011-01J8ZD….wal",
      "first_seq": 1011, "last_seq": 1018, "bytes": 131072, "crc32c": 2882400001 }
  ],
  "segments": [
    { "id": "01J8Z9…", "key_prefix": "…/segments/01J8Z9…/",
      "external": false, "level": 1,
      "docs": 100000, "live_docs": 99873, "bytes": 73400320,
      "min_seq": 0, "max_seq": 1002,
      "tombstone_bitmaps": ["…/dels/01J8ZE….bmp"],
      "features_required": [], "features_optional": [],
      "stats": { "attrs": { "stars": {"min": 0, "max": 5, "ndv_est": 6} } } }
  ],
  "parent": { "ns_id": "01J8Y0…", "generation": 17,
              "key_prefix": "…/namespaces/01J8Y0…/" },     // branches only
  "idempotency_window": { "since": "2025-05-31T12:00:00Z",
                          "keys_crc_filter_key": null },
  "counters": { "docs_live_est": 99891, "wal_bytes_pending": 655360 }
}
```

Invariants:

- WAL ranges are contiguous and disjoint: `wal[i].last_seq + 1 == wal[i+1].first_seq`,
  and `wal.last().last_seq + 1 == next_wal_seq` (empty list allowed).
- Segment `[min_seq, max_seq]` ranges and WAL ranges together cover `[0, next_wal_seq)`
  with no gaps; overlapping segment ranges are permitted only across levels
  (LSM-style shadowing; higher `max_seq` wins per document).
- `external: true` segments/WAL entries reference another namespace's prefix
  (branching). Their objects MUST NOT be deleted by this namespace's GC.

## 5. WAL chunk format (`.wal`)

```
┌──────────────────────────────────────────────────────┐
│ header                                                │
│   magic        8  bytes  "GNTWAL\0\1"                 │
│   version      u16                                    │
│   header_len   u32      (bytes from offset 0)         │
│   ns_id        16 bytes (ULID binary)                 │
│   first_seq    u64                                    │
│   record_count u32                                    │
│   flags        u32      bit0 = body_zstd              │
│   idem_count   u16      then idem_count entries:      │
│     key_len u16, key bytes, request_crc32c u32        │
├──────────────────────────────────────────────────────┤
│ body: record_count frames (zstd-compressed if flag)   │
│   frame: len u32 | crc32c u32 | payload (len bytes)   │
│     crc32c covers payload only                        │
├──────────────────────────────────────────────────────┤
│ footer: body_crc32c u32 | total_len u64 | magic again │
└──────────────────────────────────────────────────────┘
```

Frame payload is a MessagePack-encoded record. Record `seq` is implicit:
`first_seq + frame_index`. Record kinds:

```
{ "k": "upsert",  "id": "...", "vec": <bin f32 LE>|null,
  "svec": {"i": [u32...], "v": [f32...]}|null, "attrs": { ... } }
{ "k": "patch",   "id": "...", "set": { ... }, "unset": ["a", ...],
  "if_version": u64|null }
{ "k": "delete",  "id": "..." }
{ "k": "delete_resolved", "ids": ["...", ...], "origin_filter_crc": u32 }
{ "k": "config",  "change": { ... } }            // e.g. first-vector dim fixation
```

Rules:

- A chunk is written with `put_if_not_exists`; a chunk not referenced by any
  manifest MUST be treated as nonexistent (garbage).
- Readers MUST verify frame CRCs and the footer; any mismatch → `corrupt_object`
  error naming the key (immutable objects make this an object-store integrity
  event, not a recovery situation).
- Conditional results (`if_version` failures) are resolved at commit time by the
  writer; failed rows are **not** written to the WAL (the WAL contains only
  applied operations), keeping replay unconditional and deterministic.

## 6. Segment formats

### 6.1 `seg.meta.json` (immutable JSON)

```json
{
  "format_version": 1, "segment_id": "01J8Z9…",
  "docs": 100000, "min_seq": 0, "max_seq": 1002, "level": 1,
  "features_required": [], "features_optional": ["vec_raw_retained"],
  "files": {
    "docs.col": { "bytes": 5242880, "crc32c": 1, "sections": { "...": "see §6.2" } },
    "vec.ivf":  { "bytes": 314572800, "crc32c": 2, "sections": { "...": "see §6.3" } },
    "fts.idx":  { "bytes": 8388608,  "crc32c": 3 },
    "attr.idx": { "bytes": 1048576,  "crc32c": 4 }
  },
  "ordinal_index": { "id_hash_fst_offset": 0 },
  "stats": { "attrs": { "stars": {"min":0,"max":5,"ndv_est":6} },
             "fts": {"total_tokens": 12345678, "fields": {"body": {"avg_len": 213.4}}} }
}
```

Documents within a segment are addressed by **ordinal** `[0, docs)`, assigned in
ascending `(id)` order. Every file's header repeats `segment_id` so a misplaced
object can never be mis-joined. Each file ends with a **section directory**: a
footer listing `(section_name_hash u64, offset u64, len u64)` triples plus
`dir_crc32c u32` and `dir_len u32`, enabling single-range-read discovery of any
section (`GET` last 4 KiB → parse directory → range-read sections).

### 6.2 Document column store (`docs.col`)

Sections (each independently fetchable by range):

| Section | Content |
|---|---|
| `ids` | FST mapping doc id → ordinal; plus reverse offsets array (`u32`/`u64`) |
| `versions` | per-ordinal `u64` WAL seq (delta + bitpacked) |
| `attr:{name}` | one column per attribute: type tag, null bitmap (roaring), then plain/dictionary/bitpacked values; strings = offsets + bytes; arrays = lengths + flattened values |
| `vec_raw` | optional raw float32 vectors (when index stores normalized copies) |
| `payload_spill` | non-filterable nested JSON attributes, MessagePack, per-ordinal offsets |

### 6.3 IVF vector index (`vec.ivf`)

Sections:

| Section | Content |
|---|---|
| `params` | dim u32, metric u8 (0=cosine,1=dot,2=l2), nlist u32, encoding u8 (0=f32,1=i8 scalar-quantized), trained_on u32 |
| `centroids` | nlist × dim float32, row-major — small enough to pin in RAM |
| `list_dir` | nlist × (offset u64, count u32) into `lists` |
| `lists` | per-centroid contiguous runs of (ordinal u32, vector payload). i8 encoding stores per-list scale/bias f32 pairs first |

A probe = read `centroids` once (cached) + `nprobe` contiguous range reads from
`lists`. `nlist` defaults to `clamp(round(sqrt(docs)), 16, 65536)`; segments with
`docs < 10_000` MAY omit `vec.ivf` entirely (planner falls back to exact over
`vec_raw`/`docs.col`).

### 6.4 Full-text index (`fts.idx`)

Sections:

| Section | Content |
|---|---|
| `fields` | field table: name, id u16, avg_len f32, total_docs u32 |
| `terms` | FST: term bytes → (field_mask u16, postings offset u64, doc_freq u32) per field block |
| `postings` | blocks of 128 doc ordinals (delta + bitpacked) with matching term frequencies (bitpacked); skip entries every block: (last_ordinal u32, offset u64) |
| `norms` | per-ordinal per-field length quantized u8 (BM25 length normalization) |

Sparse vectors share this structure under reserved field id `0xFFFF` with term =
big-endian u32 index bytes and f32 weights replacing term frequencies.

### 6.5 Attribute index (`attr.idx`)

Per filterable attribute: roaring bitmap per dictionary value for low-cardinality
strings/bools; sorted (value → ordinal-run) ranges for numerics enabling
`gt/gte/lt/lte` via binary search; zone maps (min/max per 4 Ki-ordinal zone) for
everything. Section names: `bmp:{attr}`, `rng:{attr}`, `zone:{attr}`.

## 7. Tombstone bitmaps (`dels/{ulid}.bmp`)

When deletes/updates land *after* a segment is built, indexing emits sidecar
bitmaps rather than rewriting segments:

```
magic "GNTDEL\0\1" | version u16 | header_len u32
segment_id 16B | covers_min_seq u64 | covers_max_seq u64
roaring bitmap (portable format) of dead ordinals | crc32c u32
```

The manifest attaches bitmap keys to each segment; readers OR all bitmaps for a
segment. Compaction folds them in and drops them.

## 8. Branch registry (`branches.json`)

CAS-replaced JSON listing direct children:

```json
{ "format_version": 1,
  "children": [ { "ns_id": "01J8ZF…", "org": "o1", "project": "p1",
                   "forked_generation": 17, "created_at": "…" } ] }
```

GC for a namespace MUST consult its registry transitively before deleting any
object; an object is deletable only if unreachable from the live manifest of this
namespace **and** every descendant's live manifest, and older than the grace
window (default 24 h, measured from when it became unreachable, recorded in a GC
scratch file `gc/pending.json`).

## 9. Garbage collection protocol

1. List `manifests/`; all generations older than current minus `manifest_history`
   (default 10) are deletion candidates (kept briefly for debugging/time-travel).
2. Build the live set: objects referenced by the current manifest + descendant
   manifests (via `branches.json`, recursive) + manifests in the retained history.
3. Diff against listed objects under the namespace prefix; unreferenced objects
   enter `gc/pending.json` with a first-seen timestamp (CAS-updated).
4. On a later pass, pending objects older than the grace window are deleted —
   re-checking reachability first.
5. GC never deletes under another namespace's prefix, and never deletes `NSROOT`
   except in `deleted`-state final cleanup.

## 10. Catalog objects

`catalog/orgs/{org}.json`, `project.json` (per project) and `catalog/keys/{key_id}.json`
are small CAS-replaced JSON documents:

```json
// project.json
{ "format_version": 1, "org_id": "o1", "project_id": "p1",
  "namespaces": { "prod-docs": { "ns_id": "01J8ZB…", "created_at": "…" } } }

// keys/{key_id}.json   — key_id is public; the secret is never stored
{ "format_version": 1, "key_id": "gk_live_01J8…",
  "secret_sha256": "base64…", "salt": "base64…",
  "role": "writer", "scope": { "org": "o1", "project": "p1" },
  "created_at": "…", "disabled": false }
```

API keys present as `Authorization: Bearer gk_<env>_<key_id>.<secret>`; the server
verifies `sha256(salt || secret)`. Secrets appear in no object, log, or metric.

## 11. Limits (format-level)

| Limit | Value |
|---|---|
| Document id | ≤ 256 bytes UTF-8 |
| Vector dim | 1 … 8192 (fixed per namespace) |
| Attributes per document | ≤ 256 filterable; spill unlimited ≤ 1 MiB/doc |
| WAL chunk | ≤ 64 MiB |
| Write batch | ≤ 32 MiB serialized, ≤ 10 000 documents |
| Segment | ≤ 8 Gi bytes per file, ≤ 16 Mi documents |
| Manifest | ≤ 16 MiB (bounds segment count ≈ tens of thousands) |
