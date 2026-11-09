# Shoal Python SDK

The official Python client for [Shoal](../../README.md) — an open-source,
object-storage-native vector + full-text search database.

- Synchronous (`Shoal`) and asynchronous (`AsyncShoal`) clients
- Fully typed request/response models (Pydantic v2), ships `py.typed`
- Automatic retries with exponential backoff, jitter, and `Retry-After` support
- Batch ingestion helpers bounded by document count *and* payload size
- Composable, type-safe metadata filters

Supports Python 3.9+. Depends only on `httpx` and `pydantic`.

## Installation

```bash
pip install shoal-client
# or from this repository:
pip install -e sdks/python
```

## Quickstart

```python
from shoal import Shoal, Field

client = Shoal(api_key="sk-...", base_url="http://localhost:8780")

# Create a namespace for 3-dimensional cosine vectors
client.create_namespace("articles", dimensions=3, distance_metric="cosine")
ns = client.namespace("articles")

# Upsert documents (rows)
ns.upsert(documents=[
    {"id": "a1", "vector": [0.1, 0.2, 0.3],
     "attributes": {"title": "Object storage 101", "lang": "en", "stars": 412}},
    {"id": "a2", "vector": [0.0, 0.9, 0.1],
     "attributes": {"title": "Stockage objet", "lang": "fr", "stars": 12}},
])

# Vector search with a metadata filter
results = ns.query(
    vector=[0.1, 0.2, 0.3],
    filter=(Field("lang") == "en") & (Field("stars") >= 100),
    top_k=5,
    include_attributes=["title"],
)
for match in results:
    print(match.id, match.score, match.attributes.get("title"))
```

`api_key` and `base_url` default to the `SHOAL_API_KEY` and `SHOAL_BASE_URL`
environment variables.

## Full-text and hybrid search

```python
# BM25 full-text search with per-field boosts
ns.query(text="object storage", text_fields={"title": 2.0, "body": 1.0}, top_k=10)

# Hybrid: vector + BM25, fused with reciprocal rank fusion (default)
ns.query(vector=embedding, text="object storage", top_k=10)

# Hybrid with weighted score fusion
ns.query(vector=embedding, text="object storage",
         fusion="weighted", vector_weight=0.7, text_weight=0.3)

# Several queries in one round-trip
from shoal import Query
batches = ns.multi_query([
    Query(text="compaction"),
    Query(vector=embedding, top_k=3),
])
```

## Filters

Two equivalent styles:

```python
from shoal import Field
from shoal import filters as f

# Operator style
flt = (Field("lang") == "en") & ~(Field("tags").contains_any(["draft"]))

# Function style
flt = f.and_(f.eq("lang", "en"), f.not_(f.contains_any("tags", ["draft"])))
```

Supported leaf operators: `eq`, `not_eq`, `gt`, `gte`, `lt`, `lte`, `in_`,
`contains_any`, `prefix`. Combinators: `&` / `and_`, `|` / `or_`, `~` / `not_`.

## Batch ingestion

`upsert_many` streams any iterable of documents in batches, bounded by both
document count and approximate request size:

```python
result = ns.upsert_many(
    generate_documents(),       # any iterable / generator
    batch_size=256,
    max_batch_bytes=8 * 1024 * 1024,
    on_batch=lambda i, r: print(f"batch {i}: {r.upserted} upserted"),
)
print(result.total_upserted, "documents in", result.batches, "batches")
```

Column-oriented ingestion is also supported for dense numeric workloads:

```python
ns.upsert(ids=["a", "b"], vectors=[[1.0, 0.0], [0.0, 1.0]],
          attributes={"lang": ["en", "fr"]})
```

## Deletes, patches, export

```python
ns.patch([{"id": "a1", "attributes": {"stars": 500}}])    # merge attributes
ns.delete_documents(ids=["a2"])
ns.delete_documents(filter=Field("lang") == "fr")

for doc in ns.export(batch_size=500):                      # streams all docs
    print(doc.id, doc.attributes)
```

## Branching, copying, cache warming

```python
branch = ns.branch("experiment-1")   # copy-on-write branch; instant
backup = ns.copy("articles-backup")  # full independent copy
ns.warm()                            # preload segments into local cache
ns.pin()                             # exempt from cache eviction
ns.unpin()
```

## Async client

```python
import asyncio
from shoal import AsyncShoal

async def main():
    async with AsyncShoal(api_key="sk-...") as client:
        ns = client.namespace("articles")
        results = await ns.query(text="object storage", top_k=5)
        async for doc in ns.export():
            ...

asyncio.run(main())
```

## Retries and idempotency

All requests are retried on connection errors, timeouts, and HTTP
408/429/5xx with exponential backoff (default: 3 retries, 0.25 s initial,
×2 multiplier, 8 s cap, jitter, `Retry-After` honored). Because *writes are
retried too*, pass an idempotency key on writes so a retried request is safe:

```python
from shoal import RetryConfig

client = Shoal(api_key="sk-...", retry=RetryConfig(max_retries=5, max_backoff=30.0))
ns.upsert(documents=docs, idempotency_key="ingest-2024-06-01-part-3")
```

## Error handling

```python
from shoal import NotFoundError, RateLimitError, ShoalError

try:
    ns.query(text="hello")
except NotFoundError:
    ...                          # namespace does not exist
except RateLimitError as e:
    print("retry after", e.retry_after)
except ShoalError:
    ...                          # catch-all for every SDK error
```

Hierarchy: `ShoalError` → `TransportError` | `APIError` →
`BadRequestError`, `AuthenticationError`, `AuthorizationError`,
`NotFoundError`, `ConflictError`, `RateLimitError`, `ServerError`.

## Development

```bash
cd sdks/python
pip install -e ".[dev]"
pytest            # unit tests (no server required; uses httpx.MockTransport)
ruff check shoal tests
mypy shoal
```

End-to-end tests that drive a real server live in `tests/e2e/` at the
repository root and run against the Docker Compose stack.
