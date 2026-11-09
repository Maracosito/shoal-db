# Lagoon Python SDK

Package: **`lagoon-client`** (Python ≥ 3.9). Depends only on `httpx`.

```bash
pip install lagoon-client
```

## Quickstart

```python
from lagoon import Lagoon

client = Lagoon(
    base_url="http://localhost:8080",   # or env LAGOON_URL
    api_key="lg_dev_key",               # or env LAGOON_API_KEY
)

# Create a namespace
client.create_namespace(
    "articles",
    vector_dimensions=384,
    metric="cosine",
    fulltext_fields={"title": {"weight": 2.0}, "body": {"weight": 1.0}},
    attributes={"category": "string", "year": "int", "tags": "string[]"},
)

ns = client.namespace("articles")

# Upsert (the SDK batches automatically, 1000 docs per request by default)
ns.upsert([
    {
        "id": "a1",
        "vector": [0.1] * 384,
        "fields": {"title": "Tide tables explained", "body": "..."},
        "attributes": {"category": "guides", "year": 2024, "tags": ["ocean"]},
    },
])

# Query
hits = ns.query(
    vector=[0.1] * 384,
    text="tide tables",
    fusion="rrf",
    filter=["Eq", "category", "guides"],
    top_k=5,
    include_attributes=["title", "year"],
)
for h in hits.results:
    print(h.id, h.score, h.attributes["title"])
```

## Client

```python
Lagoon(
    base_url: str | None = None,        # default: env LAGOON_URL or http://localhost:8080
    api_key: str | None = None,         # default: env LAGOON_API_KEY
    timeout: float = 30.0,
    retries: int = 3,                   # exponential backoff on 429/5xx/connection errors
)
```

| Method | Description |
|---|---|
| `create_namespace(name, *, vector_dimensions=None, metric="cosine", sparse=False, fulltext_fields=None, attributes=None)` | `PUT /v1/namespaces/{ns}` |
| `delete_namespace(name)` | `DELETE /v1/namespaces/{ns}` |
| `list_namespaces(prefix=None)` | iterator over namespace metadata |
| `namespace(name)` | returns a `Namespace` handle (no network call) |
| `health()` | `GET /healthz` |

## Namespace

| Method | Description |
|---|---|
| `meta()` | namespace metadata (`NamespaceMeta` dataclass) |
| `upsert(docs, *, batch_size=1000, columns=None, if_manifest_generation=None, idempotency_key=None)` | batched upserts; pass `columns=` for column-oriented payloads |
| `patch(docs, *, strict=False)` | partial updates |
| `delete(ids)` | delete by ID list |
| `delete_by_filter(filter)` | delete matching documents |
| `query(...)` | see below |
| `multi_query(queries)` | list of query dicts → list of `QueryResponse` |
| `export(include_vectors=False)` | generator yielding every document (handles pagination) |
| `copy(target, wait=True)` | eager copy; polls the operation when `wait` |
| `branch(target)` | O(1) copy-on-write branch; returns the new `Namespace` |
| `warm(level="indexes", wait=False)` | prefetch caches |
| `pin()` / `unpin()` | cache pinning |

### `query()`

```python
ns.query(
    vector: list[float] | None = None,
    sparse_vector: dict | None = None,        # {"indices": [...], "values": [...]}
    text: str | None = None,
    text_fields: list[str] | None = None,
    rank_by: list[dict] | None = None,        # full control; overrides the shorthands
    fusion: str | dict = "rrf",               # "rrf" | "weighted" | {"method": ..., ...}
    filter: list | None = None,               # filter AST, see API reference
    top_k: int = 10,
    include_attributes: list[str] | None = None,
    include_vectors: bool = False,
    ann: dict | None = None,                  # {"mode": "exact"} to force brute force
    min_manifest_generation: int | None = None,
) -> QueryResponse
```

`QueryResponse` fields: `.results` (list of `Hit(id, score, signals,
attributes, vector)`), `.took_ms`, `.plan`, `.manifest_generation`.

Every write method returns a `WriteResult(manifest_generation, versions, ...)`;
pass `result.manifest_generation` as `min_manifest_generation` to a query for
read-your-writes against any node.

## Errors

All API errors raise `lagoon.LagoonError` (with `.code`, `.status`,
`.request_id`). Subclasses: `NotFoundError`, `ConflictError`, `AuthError`,
`RateLimitedError` (with `.retry_after`), `ValidationError`. Connection-level
failures raise `lagoon.TransportError` after retries are exhausted.

## Async

`lagoon.AsyncLagoon` mirrors the entire API with `async`/`await` methods,
sharing the same request/response types:

```python
from lagoon import AsyncLagoon

async with AsyncLagoon() as client:
    ns = client.namespace("articles")
    resp = await ns.query(text="tide tables", top_k=3)
```

## Embeddings are your job (by design)

The SDK never calls an embedding provider. Bring vectors from any source —
see `demos/common/embeddings.py` for a pluggable provider pattern (hash-based
local embeddings for tests, `sentence-transformers`, or any HTTP provider).
