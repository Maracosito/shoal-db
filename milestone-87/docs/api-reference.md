# Lagoon HTTP API Reference

Base URL (default local deployment): `http://localhost:8080`

All requests and responses are JSON (`Content-Type: application/json`).
All endpoints are versioned under `/v1`.

## Authentication

Every request (except `/healthz`) requires an API key:

```
Authorization: Bearer <api-key>
```

Keys are scoped to an organization and carry a role:

| Role | Permissions |
|---|---|
| `reader` | query, export, namespace metadata, warm |
| `writer` | reader + upsert/patch/delete, branch, copy, pin |
| `admin`  | writer + create/delete namespaces, key management, quotas |

Keys are configured via the server config / `lagoon keys` CLI in self-hosted
deployments. Keys are never logged; audit logs record key IDs, not secrets.

## Conventions

- **Errors** use a uniform envelope:

```json
{ "error": { "code": "namespace_not_found", "message": "namespace 'docs' does not exist", "request_id": "req_01HX..." } }
```

  Status codes: `400` invalid request, `401` missing/bad key, `403` role
  denied, `404` not found, `409` conflict (CAS/conditional failure),
  `412` precondition failed, `429` rate limited (with `Retry-After`),
  `503` storage unavailable.

- **Idempotency**: write endpoints accept an `Idempotency-Key` header
  (any string ≤ 256 chars, deduplicated for 24h).
- **Pagination**: list/export endpoints return `next_cursor`; pass it back as
  `cursor`.
- Namespace names: `[a-zA-Z0-9._-]{1,128}`.

---

## Namespaces

### Create namespace
`PUT /v1/namespaces/{ns}`

```json
{
  "schema": {
    "vector": { "dimensions": 384, "metric": "cosine" },
    "sparse_vector": { "enabled": true },
    "fulltext_fields": { "title": { "weight": 2.0 }, "body": { "weight": 1.0 } },
    "attributes": { "category": "string", "price": "float", "tags": "string[]", "published": "bool", "year": "int" }
  }
}
```

`metric` ∈ `cosine | dot | euclidean`. Attribute types:
`string, int, float, bool, string[], int[], datetime`. Returns `201` with the
namespace metadata, or `200` if it already exists with an identical schema
(`409` on schema mismatch).

### Get namespace metadata
`GET /v1/namespaces/{ns}` →

```json
{
  "name": "docs", "created_at": "2025-06-01T12:00:00Z",
  "schema": { "...": "..." },
  "approx_doc_count": 12840, "approx_bytes": 73400320,
  "segments": 6, "wal_tail_chunks": 1,
  "manifest_generation": 42,
  "branch": { "parent": "docs-main", "base_generation": 38 },
  "pinned": false
}
```

### List namespaces
`GET /v1/namespaces?prefix=&cursor=&limit=100`

### Delete namespace
`DELETE /v1/namespaces/{ns}` → `204`. Shared segments referenced by branches
are retained by GC; only this namespace's exclusive data is reclaimed.

### Update namespace settings
`PATCH /v1/namespaces/{ns}` — mutable settings only (field weights, additional
attributes, `ann.nprobe_default`, compaction tuning). Vector dimensions and
metric are immutable.

---

## Documents

A document:

```json
{
  "id": "doc-123",
  "vector": [0.12, -0.05, ...],
  "sparse_vector": { "indices": [12, 904, 30001], "values": [0.8, 0.31, 0.12] },
  "fields": { "title": "Tide tables explained", "body": "..." },
  "attributes": { "category": "guides", "year": 2024, "tags": ["ocean", "tides"] }
}
```

`id` is required (string ≤ 512 bytes); everything else is optional.

### Upsert documents
`POST /v1/namespaces/{ns}/documents`

Row-oriented:

```json
{ "documents": [ { "id": "a", "...": "..." }, { "id": "b", "...": "..." } ] }
```

Column-oriented (efficient for large homogeneous batches):

```json
{
  "columns": {
    "id": ["a", "b"],
    "vector": [[...], [...]],
    "fields.title": ["Doc A", "Doc B"],
    "attributes.year": [2024, 2025]
  }
}
```

Optional conditions: top-level `"if_manifest_generation": 41`, or per-document
`"if_version": 3` (each upsert returns new per-document `version` numbers).
Response:

```json
{ "upserted": 2, "manifest_generation": 43,
  "versions": { "a": 4, "b": 1 } }
```

`409` if any condition fails (the whole batch is rejected atomically).
Max batch size: 10 MB or 10,000 documents.

### Patch documents
`PATCH /v1/namespaces/{ns}/documents` — partial updates; only provided
fields/attributes change. Same body shapes and conditions as upsert. Patching
a missing ID is reported in `"missing": [...]` and is not an error unless
`"strict": true`.

### Delete by ID
`POST /v1/namespaces/{ns}/documents/delete`

```json
{ "ids": ["a", "b"] }
```

### Delete by filter
`POST /v1/namespaces/{ns}/documents/delete`

```json
{ "filter": ["And", [["Eq", "category", "drafts"], ["Lt", "year", 2020]]] }
```

Returns `{ "deleted": <approx count>, "manifest_generation": ... }`.

---

## Filters

A filter is a JSON array AST:

```
["Eq",          attr, value]
["NotEq",       attr, value]
["Gt"|"Gte"|"Lt"|"Lte", attr, value]
["In",          attr, [v1, v2, ...]]
["ContainsAny", attr, [v1, v2, ...]]     // for array attributes
["Glob",        attr, "tide-*"]          // simple * and ? wildcards on strings
["And", [f1, f2, ...]]
["Or",  [f1, f2, ...]]
["Not", f]
```

Filters are exact pre-filters: scoring only considers matching documents.

---

## Query

`POST /v1/namespaces/{ns}/query`

```json
{
  "rank_by": [
    { "type": "vector", "vector": [/* dims floats */], "weight": 1.0 },
    { "type": "text", "query": "storm surge forecast", "fields": ["title", "body"], "weight": 1.0 },
    { "type": "sparse", "sparse_vector": { "indices": [...], "values": [...] }, "weight": 0.5 }
  ],
  "fusion": { "method": "rrf", "rrf_k": 60 },
  "filter": ["Eq", "category", "guides"],
  "top_k": 10,
  "include_attributes": ["title", "category", "year"],
  "include_vectors": false,
  "ann": { "mode": "auto", "nprobe": 16 },
  "min_manifest_generation": 43
}
```

Notes:

- A single-signal query may use the shorthand top-level `"vector": [...]` or
  `"text": "..."` instead of `rank_by`.
- `fusion.method` ∈ `rrf | weighted` (weighted uses min-max normalized scores
  × `weight`).
- `ann.mode` ∈ `auto | exact | ivf`. `exact` forces brute-force kNN
  (full recall, slower); `auto` lets the planner decide.
- `min_manifest_generation` provides cross-node read-your-writes (the server
  waits, up to a timeout, until its manifest view catches up).

Response:

```json
{
  "results": [
    { "id": "doc-9", "score": 0.0312,
      "signals": { "vector": 0.892, "text": 12.4 },
      "attributes": { "title": "...", "category": "guides", "year": 2024 } }
  ],
  "took_ms": 7.4,
  "plan": { "strategy": "ivf+fts", "segments": 4, "nprobe": 16, "candidates": 4096 },
  "manifest_generation": 43
}
```

### Multi-query
`POST /v1/namespaces/{ns}/multi-query` —
`{ "queries": [ <query>, <query>, ... ] }` (≤ 32), returns
`{ "results": [ <response>, ... ] }` with shared segment loading.

---

## Export, Copy, Branch

### Export
`GET /v1/namespaces/{ns}/export?cursor=&limit=1000&include_vectors=true` —
streams documents in stable ID order; `next_cursor` paginates. Snapshot
semantics: the manifest generation is fixed at the first page.

### Copy
`POST /v1/namespaces/{ns}/copy` → `{ "target": "ns-copy" }` — eager physical
copy; returns `202` with an `operation_id` pollable at
`GET /v1/operations/{id}`.

### Branch
`POST /v1/namespaces/{ns}/branch` → `{ "target": "ns-feature" }` — O(1)
copy-on-write branch (see architecture guide §7). Returns `201` immediately.
The branch reports its lineage in namespace metadata.

---

## Cache Control

- `POST /v1/namespaces/{ns}/warm` —
  `{ "level": "metadata" | "indexes" | "full" }` (default `indexes`). Returns
  `202` with an `operation_id`; poll for completion and bytes fetched.
- `POST /v1/namespaces/{ns}/pin` / `DELETE /v1/namespaces/{ns}/pin` — exempt
  cached files from eviction (subject to `cache.max_pinned_bytes`).

---

## Operations & Health

- `GET /healthz` — liveness, no auth: `{ "status": "ok" }`.
- `GET /readyz` — readiness: object storage reachable, cache dir writable.
- `GET /metrics` — Prometheus exposition (see deployment guide for the metric
  catalog: query latency histograms, cache hits by layer, indexing lag, WAL
  size, segment counts, compaction status, object-storage request counters).
- `GET /v1/operations/{id}` — status of async operations (copy, warm, rebuild):
  `{ "status": "running|done|failed", "progress": 0.7, "error": null }`.
- `POST /v1/namespaces/{ns}/rebuild` — rebuild all indexes from the row store /
  WAL (admin; async). `POST /v1/namespaces/{ns}/repair` verifies checksums and
  reconstructs damaged segment files where source data permits.

## Rate Limits & Quotas (optional, off by default)

When enabled in server config, limits apply per API key:
`requests_per_second`, `write_bytes_per_minute`, `max_namespaces`,
`max_docs_per_namespace`. Exceeding them returns `429` with `Retry-After`.
