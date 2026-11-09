# @shoal-db/client

Official TypeScript/JavaScript SDK for **Shoal** — the open-source, object-storage-native vector + full-text search database.

- Zero runtime dependencies (uses global `fetch`; Node ≥ 18, browsers, edge runtimes)
- Fully typed request/response models
- Automatic retries with exponential backoff, jitter, and `Retry-After` support
- Idempotency keys for safe write retries
- Batched bulk upserts with bounded concurrency and progress callbacks
- Async-iterator export pagination
- Type-safe filter expression builders

## Install

```sh
npm install @shoal-db/client
```

## Quickstart

```ts
import { ShoalClient, and, eq, gte } from "@shoal-db/client";

const client = new ShoalClient({
  baseUrl: "http://localhost:8800",
  apiKey: process.env.SHOAL_API_KEY,
});

// Create a namespace with dense vectors and weighted text fields.
await client.createNamespace("articles", {
  vector_dims: 384,
  metric: "cosine",
  text_fields: { title: 2.0, body: 1.0 },
  indexed_attributes: ["lang", "stars"],
});

const ns = client.namespace("articles");

// Upsert documents.
await ns.upsert([
  {
    id: "a-1",
    vector: [/* 384 floats */],
    text: { title: "Object storage as a database", body: "..." },
    attributes: { lang: "en", stars: 412 },
  },
]);

// Hybrid query: vector + BM25 with reciprocal rank fusion, filtered.
const res = await ns.query({
  vector: queryEmbedding,
  text: "durable search on S3",
  top_k: 10,
  fusion: { method: "rrf" },
  filter: and(eq("lang", "en"), gte("stars", 100)),
  include_attributes: ["title", "lang"],
});

for (const hit of res.results) {
  console.log(hit.id, hit.score, hit.attributes);
}
```

## Bulk loading

`upsertBatched` chunks documents by count **and** approximate payload size, sends batches with bounded concurrency, and (optionally) tags each batch with a deterministic idempotency key so retried batches are never applied twice:

```ts
const { upserted } = await ns.upsertBatched(documents, {
  batchSize: 500,
  maxBatchBytes: 8 * 1024 * 1024,
  concurrency: 4,
  idempotencyPrefix: `load-${Date.now()}`,
  onProgress: (p) =>
    console.log(`${p.documentsDone}/${p.documentsTotal} documents`),
});
```

Column-oriented ingestion avoids per-row object overhead for large loads:

```ts
await ns.upsertColumns({
  ids: ["1", "2"],
  vectors: [v1, v2],
  attributes: { lang: ["en", "de"] },
});
```

## Filters

Filters are plain JSON; builders make them type-safe:

```ts
import { and, or, not, eq, isIn, containsAny, prefix, gte } from "@shoal-db/client";

const f = and(
  eq("lang", "en"),
  or(gte("stars", 100), containsAny("tags", ["featured"])),
  not(isIn("status", ["archived"])),
  prefix("path", "docs/"),
);
```

Supported operators: `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in`, `contains_any`, `prefix`, plus `and` / `or` / `not` combinators.

## Export

```ts
for await (const doc of ns.exportDocuments({ pageSize: 1000 })) {
  process.stdout.write(JSON.stringify(doc) + "\n");
}
```

## Branching, warming, pinning

```ts
await ns.branchTo("articles-experiment");   // copy-on-write branch
await client.namespace("articles-experiment").warm(); // preload caches
await ns.pin();                              // keep hot, never evicted
```

Branch writes never affect the source and vice versa; shared immutable segments are reference-counted server-side.

## Errors and retries

All SDK errors extend `ShoalError`. API failures map to status-specific
classes (`ValidationError`, `AuthenticationError`, `NotFoundError`,
`ConflictError`, `RateLimitError`, `ServerError`).

Retry behavior:

- `GET`/`PUT`/`DELETE` are retried on 408/425/429/5xx and network errors.
- `POST`/`PATCH` are retried only on 429, **or** on any retryable failure when
  an `idempotencyKey` is supplied.
- `Retry-After` headers are honored; otherwise exponential backoff with jitter
  (base 250 ms, ceiling 10 s, 3 retries by default).

```ts
import { RateLimitError, ShoalError } from "@shoal-db/client";

try {
  await ns.query({ text: "hello" });
} catch (err) {
  if (err instanceof RateLimitError) {
    console.warn(`rate limited; retry in ${err.retryAfterMs} ms`);
  } else if (err instanceof ShoalError) {
    console.error(err.message);
  }
}
```

## Configuration

```ts
new ShoalClient({
  baseUrl: "http://localhost:8800", // default
  apiKey: "...",                    // sent as Authorization: Bearer
  timeoutMs: 30_000,
  maxRetries: 3,
  retryBaseDelayMs: 250,
  retryMaxDelayMs: 10_000,
  headers: { "x-team": "search" },  // extra headers on every request
  fetch: customFetch,               // inject for testing / instrumentation
});
```

## Development

```sh
npm install
npm run build   # tsc -> dist/
npm test        # vitest unit tests (no server required)
npm run lint    # strict typecheck across src + tests
```

End-to-end tests that drive a live server live in `tests/e2e` at the
repository root and run in CI against the Docker Compose stack.

## License

Apache-2.0
