# shoal-it — integration-test suite

This crate spins up the real `shoal-server` router in-process on an ephemeral
port and drives it over HTTP, exactly like an external client.

## Running

```sh
# Local-filesystem backend (no external services needed):
cargo test -p shoal-it

# MinIO / S3 backend tests (skipped unless the endpoint is set):
docker compose up -d minio
# Create the bucket once (mc, the console, or aws cli):
#   mc alias set local http://127.0.0.1:9000 minioadmin minioadmin
#   mc mb local/shoal-it
SHOAL_IT_S3_ENDPOINT=http://127.0.0.1:9000 cargo test -p shoal-it --test minio
```

Environment variables for the S3 suite:

| Variable                 | Default      | Notes                          |
| ------------------------ | ------------ | ------------------------------ |
| `SHOAL_IT_S3_ENDPOINT`   | *(unset)*    | Required to enable S3 tests    |
| `SHOAL_IT_S3_BUCKET`     | `shoal-it`   | Must already exist             |
| `SHOAL_IT_S3_ACCESS_KEY` | `minioadmin` |                                |
| `SHOAL_IT_S3_SECRET_KEY` | `minioadmin` |                                |
| `SHOAL_IT_S3_REGION`     | `us-east-1`  |                                |

Each S3 run isolates itself under a fresh unique key prefix, so concurrent
runs against the same bucket do not collide.

## Test inventory

| File             | Covers |
| ---------------- | ------ |
| `branching.rs`   | Branch sees source at branch point; isolation source→branch and branch→source; multi-level branches; deleting a branch keeps the source intact; deleting the source keeps the branch queryable and writable (refcount safety); vector/text/filtered queries on branches |
| `cache.rs`       | Cold restart recovery from object storage; warm-cache endpoint correctness and 404 behaviour; cache-hit metrics under repeated identical queries; disk-cache population after warm; pin/unpin round-trip |
| `api_surface.rs` | Health & Prometheus metrics (incl. secret-safety); API-key auth and admin/writer/reader role enforcement; namespace CRUD/list/404s; upsert/patch/delete-by-id/delete-by-filter; vector search under cosine/dot/euclidean; BM25; hybrid RRF and weighted fusion; eq/in/gt/gte/lt/and/not filters; attribute projection; top_k; copy-namespace independence; error handling for bad filters, missing IDs, missing namespaces, and name collisions |
| `minio.rs`       | The condensed full surface plus branch refcount safety and cold-restart recovery, all against real S3-compatible storage |

## Harness contract

The harness (`src/lib.rs`) is the single place encoding assumptions about the
server's public surface. If the server changes any of the following, update
the harness — not the tests:

1. `shoal_server::config::ServerConfig: serde::Deserialize` parsed from the
   TOML rendered by `render_config` (storage/cache/auth/rate_limit/audit
   sections).
2. `shoal_server::build_app(ServerConfig) -> anyhow::Result<axum::Router>`.
3. Route paths in the `api` module.
4. JSON shapes produced by the `filters`/`queries` builders and `doc`/`corpus`.

Response parsing is intentionally tolerant (hits under `results`/`matches`/
`hits`/`documents`; export as JSON array, object wrapper, or NDJSON) so that
cosmetic response changes don't invalidate the behavioural assertions.
