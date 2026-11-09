# Lagoon TypeScript / JavaScript SDK

Package: **`@lagoon-db/client`** — works in Node ≥ 18 (uses global `fetch`),
Deno, Bun, and modern browsers. Zero runtime dependencies. Ships ESM + CJS +
type declarations.

```bash
npm install @lagoon-db/client
```

## Quickstart

```ts
import { Lagoon } from "@lagoon-db/client";

const client = new Lagoon({
  baseUrl: "http://localhost:8080",   // or env LAGOON_URL
  apiKey: "lg_dev_key",               // or env LAGOON_API_KEY
});

await client.createNamespace("articles", {
  vector: { dimensions: 384, metric: "cosine" },
  fulltextFields: { title: { weight: 2.0 }, body: { weight: 1.0 } },
  attributes: { category: "string", year: "int", tags: "string[]" },
});

const ns = client.namespace("articles");

await ns.upsert([
  {
    id: "a1",
    vector: new Array(384).fill(0.1),
    fields: { title: "Tide tables explained", body: "..." },
    attributes: { category: "guides", year: 2024, tags: ["ocean"] },
  },
]);

const res = await ns.query({
  vector: new Array(384).fill(0.1),
  text: "tide tables",
  fusion: "rrf",
  filter: ["Eq", "category", "guides"],
  topK: 5,
  includeAttributes: ["title", "year"],
});

for (const hit of res.results) {
  console.log(hit.id, hit.score, hit.attributes?.title);
}
```

## Client options

```ts
new Lagoon({
  baseUrl?: string;        // default env LAGOON_URL or http://localhost:8080
  apiKey?: string;         // default env LAGOON_API_KEY
  timeoutMs?: number;      // default 30_000 (per request, via AbortController)
  retries?: number;        // default 3; exponential backoff on 429/5xx/network
  fetch?: typeof fetch;    // bring your own fetch (testing, polyfills)
});
```

## Client methods

| Method | Description |
|---|---|
| `createNamespace(name, schema)` | `PUT /v1/namespaces/{ns}` |
| `deleteNamespace(name)` | delete a namespace |
| `listNamespaces(opts?)` | async iterator over namespace metadata |
| `namespace(name)` | returns a `Namespace` handle |
| `health()` | liveness check |

## `Namespace` methods

| Method | Description |
|---|---|
| `meta()` | namespace metadata |
| `upsert(docs, opts?)` | batched upserts (`batchSize` default 1000, `columns`, `ifManifestGeneration`, `idempotencyKey`) |
| `patch(docs, opts?)` | partial updates |
| `delete(ids)` | delete by ID |
| `deleteByFilter(filter)` | delete matching documents |
| `query(q)` | run a query (see below) |
| `multiQuery(queries)` | up to 32 queries in one request |
| `export(opts?)` | `AsyncIterable<Document>` over all documents |
| `copy(target, opts?)` | eager copy (`wait: true` polls the operation) |
| `branch(target)` | O(1) copy-on-write branch |
| `warm(opts?)` | prefetch caches (`level: "metadata" \| "indexes" \| "full"`) |
| `pin()` / `unpin()` | cache pinning |

### Query shape

```ts
interface QueryRequest {
  vector?: number[];
  sparseVector?: { indices: number[]; values: number[] };
  text?: string;
  textFields?: string[];
  rankBy?: RankSignal[];                      // full control; overrides shorthands
  fusion?: "rrf" | "weighted" | { method: "rrf" | "weighted"; rrfK?: number };
  filter?: Filter;                            // JSON array AST, typed
  topK?: number;                              // default 10
  includeAttributes?: string[];
  includeVectors?: boolean;
  ann?: { mode?: "auto" | "exact" | "ivf"; nprobe?: number };
  minManifestGeneration?: number;
}
```

The `Filter` type is fully typed:

```ts
type Filter =
  | ["Eq" | "NotEq" | "Gt" | "Gte" | "Lt" | "Lte", string, Scalar]
  | ["In" | "ContainsAny", string, Scalar[]]
  | ["Glob", string, string]
  | ["And" | "Or", Filter[]]
  | ["Not", Filter];
```

### Responses

`query()` resolves to:

```ts
interface QueryResponse {
  results: Array<{
    id: string;
    score: number;
    signals?: Record<string, number>;
    attributes?: Record<string, unknown>;
    vector?: number[];
  }>;
  tookMs: number;
  plan: { strategy: string; segments: number; nprobe?: number };
  manifestGeneration: number;
}
```

Write methods resolve to `{ manifestGeneration, versions, upserted }` — pass
`manifestGeneration` as `minManifestGeneration` in a follow-up query for
read-your-writes against any node.

## Errors

All API errors throw `LagoonError` with `.code`, `.status`, `.requestId`.
Narrow with `instanceof`: `NotFoundError`, `ConflictError`, `AuthError`,
`RateLimitedError` (`.retryAfter` seconds), `ValidationError`. Network
failures after retries throw `TransportError`.

## Streaming exports

```ts
for await (const doc of ns.export({ includeVectors: true })) {
  process.stdout.write(JSON.stringify(doc) + "\n");
}
```

## Notes

- The SDK never embeds text — bring vectors from any embedding provider.
- All methods accept an optional `{ signal: AbortSignal }` for cancellation.
- The JSON wire format uses `snake_case`; the SDK converts to/from
  `camelCase` at the boundary, so the TypeScript surface is idiomatic.
