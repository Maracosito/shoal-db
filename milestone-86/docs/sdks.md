# Shoal SDKs

Official clients for the Shoal HTTP API. Both SDKs cover the full API surface
and are exercised against a live server by the end-to-end suites in
[`e2e/`](../e2e/README.md) — those tests double as living usage examples.

| Capability | Python (`shoal`) | TypeScript (`shoal`) |
|---|---|---|
| Namespaces: create / list / metadata / delete | ✅ | ✅ |
| Upsert / patch / delete by ID / delete by filter | ✅ | ✅ |
| Vector, full-text, hybrid query (weighted & RRF fusion) | ✅ | ✅ |
| Metadata filter DSL | ✅ | ✅ |
| Attribute projection, top-k | ✅ | ✅ |
| Export (streaming) | ✅ | ✅ |
| Branch / copy namespaces | ✅ | ✅ |
| Warm cache / pin | ✅ | ✅ |
| Automatic retries with backoff | ✅ | ✅ |
| Batch helpers for large ingests | ✅ | ✅ |
| Async support | ✅ (`shoal.aio`) | ✅ (Promise-based) |
| Typed models | ✅ (`py.typed`) | ✅ (TypeScript) |

Full per-SDK documentation: [Python README](../sdks/python/README.md) ·
[TypeScript README](../sdks/typescript/README.md). The wire protocol is
specified in [`docs/api.md`](api.md) and the OpenAPI spec.

## Python

### Install

```bash
pip install -e sdks/python        # from the repo (PyPI release planned)
```

Supports Python 3.10+. The only runtime dependency is `httpx`.

### Quickstart

```python
from shoal import Client, Document
from shoal.filters import F

client = Client(base_url="http://localhost:8080", api_key="dev-root-key")

client.create_namespace("articles")
ns = client.namespace("articles")

ns.upsert([
    Document(
        id="a-1",
        vector=[0.1, 0.9, 0.0, 0.0],
        attributes={"title": "Vector search 101", "year": 2024, "tags": ["ai"]},
    ),
])

res = ns.query(
    vector=[0.0, 1.0, 0.0, 0.0],
    text="vector search",          # adding text makes this a hybrid query
    fusion="rrf",
    filter=F.gte("year", 2023) & F.contains_any("tags", ["ai"]),
    top_k=10,
    include_attributes=["title"],
)
for hit in res.results:
    print(hit.id, hit.score, hit.attributes)
```

### Async

```python
from shoal.aio import AsyncClient

async with AsyncClient(base_url="http://localhost:8080", api_key="...") as client:
    ns = client.namespace("articles")
    res = await ns.query(text="vector search", top_k=5)
```

### Batching and retries

- `shoal.batch` helpers split large document iterables into size-bounded
  upsert batches and run them with bounded concurrency — use them for any
  ingest beyond a few thousand documents.
- The transport retries idempotent requests on connection errors, timeouts,
  HTTP 429, and 5xx, with exponential backoff and jitter. Retry counts and
  timeouts are constructor arguments on `Client` / `AsyncClient`.

### Errors

All errors derive from `shoal.errors.ShoalError`; the important subclasses are
`AuthError` (401/403), `NotFoundError` (404), `ValidationError` (400/422), and
`RateLimitError` (429). Each carries the HTTP status and the server's error
message.

## TypeScript / JavaScript

### Install

```bash
npm install ./sdks/typescript     # from the repo (npm release planned)
```

Requires Node 18+ (uses the global `fetch`); works in modern bundlers too.

### Quickstart

```ts
import { ShoalClient, F } from "shoal";

const client = new ShoalClient({
  baseUrl: "http://localhost:8080",
  apiKey: "dev-root-key",
});

await client.createNamespace("articles");
const ns = client.namespace("articles");

await ns.upsert([
  {
    id: "a-1",
    vector: [0.1, 0.9, 0.0, 0.0],
    attributes: { title: "Vector search 101", year: 2024, tags: ["ai"] },
  },
]);

const res = await ns.query({
  vector: [0.0, 1.0, 0.0, 0.0],
  text: "vector search",
  fusion: "rrf",
  filter: F.gte("year", 2023).and(F.containsAny("tags", ["ai"])),
  topK: 10,
  includeAttributes: ["title"],
});
for (const hit of res.results) {
  console.log(hit.id, hit.score, hit.attributes);
}
```

### Batching, retries, errors

- Batch helpers in the SDK chunk large arrays into bounded upsert requests
  with configurable concurrency.
- The transport retries idempotent requests on network failures, 429, and
  5xx with exponential backoff; configure via the client constructor options.
- Errors are typed (`ShoalError` and subclasses) and include the HTTP status
  and server message; rejections are normal Promise rejections.

## Choosing filter operators

Both SDKs expose the same filter DSL, mirroring the HTTP API:

| Operator | Python | TypeScript |
|---|---|---|
| Equals | `F.eq(field, v)` | `F.eq(field, v)` |
| Greater / less (or equal) | `F.gt / F.gte / F.lt / F.lte` | same |
| Membership | `F.in_(field, [...])` | `F.in_(field, [...])` |
| Array overlap | `F.contains_any(field, [...])` | `F.containsAny(field, [...])` |
| Conjunction | `a & b` | `a.and(b)` |
| Disjunction | `a \| b` | `a.or(b)` |
| Negation | `~a` | `a.not()` |

## Versioning

Both SDKs follow semver and track the server's API version; a given minor SDK
release supports the matching server minor and one back. Breaking wire-format
changes only happen in major releases.
