# Gannet

**An object-storage-native vector + full-text search database for AI and RAG applications.**

Gannet is an open-source search engine whose durable source of truth is S3-compatible
object storage. Compute nodes are stateless: they hydrate working sets from object
storage into local NVMe and memory caches on demand, which makes namespaces cheap when
cold and fast when warm.

> **Project status:** Milestone 1 — design package and buildable scaffold.
> The architecture, storage format, and API are specified; the Rust workspace compiles
> with working filesystem and S3 storage backends. Query/indexing engines land in later
> milestones.

## Why Gannet?

Modern AI applications create *many* small-to-medium search indexes (one per user,
per tenant, per agent, per document set), most of which are idle most of the time.
Traditional search engines keep everything resident on expensive replicated disks.
Gannet instead treats object storage as the database:

- **Cold is nearly free.** A namespace at rest is just objects in a bucket
  (~$0.02/GB-month on S3). No replicas, no idle nodes holding it.
- **Warm is fast.** First query hydrates segments into local SSD/memory caches;
  subsequent queries serve from cache.
- **Stateless compute.** Any node can serve any namespace. Scale-out, upgrades,
  and recovery are trivial because nodes own no durable state.
- **Branching is O(1).** Copy-on-write namespace branches reference existing
  immutable segments — fork a 10M-document namespace in milliseconds.

## Core capabilities (v1 scope)

| Area | Features |
|------|----------|
| Vector search | Exact kNN, IVF (centroid-partitioned) ANN, cosine / dot / Euclidean |
| Full-text search | Tokenization, inverted index, BM25, field boosts |
| Hybrid search | Weighted score fusion and reciprocal rank fusion, multi-query requests |
| Filters | `eq, ne, gt, gte, lt, lte, in, contains_any, and, or, not`, prefix match |
| Writes | Durable WAL on object storage, batched upserts, patch, delete-by-id, delete-by-filter, idempotency keys, conditional writes |
| Namespaces | Create/update/delete, export, copy, **copy-on-write branch**, warm/pin |
| Ops | Prometheus metrics, structured logs, OpenTelemetry hooks, audit log |
| Clients | HTTP JSON API (OpenAPI 3.1), Python SDK, TypeScript SDK, CLI |

## Repository layout

```
docs/                  Design documents (architecture, storage format, naming, non-goals)
api/                   OpenAPI 3.1 specification
crates/gannet-core/    Storage abstraction, formats, engine (Rust)
crates/gannet-server/  HTTP API server (Rust, axum)
crates/gannet-cli/     Developer CLI (Rust)
deploy/                Docker Compose (MinIO), Kubernetes manifests (later milestone)
clients/               Python and TypeScript SDKs (later milestone)
```

## Quickstart (current milestone)

```bash
# Start MinIO for local development
docker compose -f deploy/docker-compose.yml up -d minio

# Build the workspace
cargo build --workspace

# Run storage backend tests (filesystem + MinIO integration)
cargo test --workspace
GANNET_TEST_S3=1 cargo test -p gannet-core --features s3-tests

# Start the API server skeleton against the local filesystem backend
cargo run -p gannet-server -- --storage file:///tmp/gannet-data --listen 127.0.0.1:8718
curl http://127.0.0.1:8718/v1/health
```

## Design documents

- [Architecture](docs/architecture.md) — components, write path, query path,
  consistency model, caching, branching, tradeoff analysis.
- [Storage format specification](docs/storage-format.md) — exact on-object-storage
  layout, binary formats, versioning and compatibility rules.
- [API specification](api/openapi.yaml) — OpenAPI 3.1 for every endpoint.
- [Naming & clean-room statement](docs/naming-and-branding.md)
- [Non-goals for v1](docs/non-goals-v1.md)

## Clean-room statement

Gannet is an original, greenfield implementation *inspired by the public product
category* pioneered by systems such as Turbopuffer (object-storage-native search).
It contains no code, documentation text, branding, or proprietary assets from any
other product, and its API and storage format are independently designed. See
[docs/naming-and-branding.md](docs/naming-and-branding.md).

## License

Apache-2.0.
