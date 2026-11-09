# Contributing to Lagoon

Thanks for considering a contribution! Lagoon is an Apache-2.0-licensed,
clean-room project — please read the legal note below before contributing.

## Legal ground rules

- **Do not** contribute code, docs, or formats derived from proprietary search
  databases (including reading their non-public source or decompiled
  artifacts). Public blog posts, papers, and documented behavior are fine as
  inspiration; copying is not.
- All contributions are accepted under Apache-2.0. We use the
  [Developer Certificate of Origin](https://developercertificate.org/) — sign
  off your commits with `git commit -s`.

## Development setup

Prerequisites: recent stable Rust (via `rustup`), Docker (for MinIO
integration tests), Python ≥ 3.9 (SDK + demos), Node ≥ 18 (TS SDK).

```bash
git clone https://github.com/lagoon-db/lagoon && cd lagoon

# Build everything
cargo build --workspace

# Fast unit tests (filesystem storage backend, no Docker needed)
cargo test --workspace

# Integration tests against MinIO
docker compose -f deploy/docker-compose.minio-test.yml up -d
LAGOON_TEST_S3=1 cargo test --workspace --features s3-tests
docker compose -f deploy/docker-compose.minio-test.yml down -v

# Python SDK
cd sdks/python && pip install -e ".[dev]" && pytest

# TypeScript SDK
cd sdks/typescript && npm install && npm test && npm run build
```

Run a local stack with `cargo run -p lagoon-server -- --dev` (filesystem
storage in `./.lagoon-dev`, embedded indexer, a dev API key printed on boot)
or the full Compose stack in `deploy/`.

## Repository layout

```
crates/
  lagoon-storage/    object-store abstraction, WAL, segments, manifests, GC
  lagoon-index/      IVF, inverted index/BM25, attribute indexes
  lagoon-query/      planner, executors, fusion, filters
  lagoon-server/     HTTP API, auth, cache hierarchy, metrics
  lagoon-worker/     flush, compaction, rebuild/repair
  lagoon-cli/        `lagoon` command-line tool
sdks/python/         lagoon-client
sdks/typescript/     @lagoon-db/client
demos/               runnable demos with bundled datasets
benchmarks/          benchmark suite (see docs/benchmark-guide.md)
deploy/              Docker Compose, Kubernetes manifests, Helm chart
docs/                this documentation set
tests/recovery/      crash-recovery and corruption-handling tests
```

## Code standards

- `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` must pass.
- Python: `ruff check` + `ruff format`; type hints required in the SDK.
- TypeScript: `eslint` + `prettier`; no `any` in the public API surface.
- Every behavior change needs tests. Storage-format changes need a format
  version bump and a backward-compat read test.
- Public API changes (HTTP, SDKs) need an OpenAPI spec update
  (`api/openapi.yaml`) and doc updates in the same PR.

## Submitting changes

1. Open an issue first for anything non-trivial — especially storage-format or
   API changes, which need design discussion.
2. Fork, branch (`feat/...`, `fix/...`), commit with sign-off. Conventional
   commit prefixes (`feat:`, `fix:`, `docs:`, `perf:`, `test:`) appreciated.
3. PRs should be focused and include: what/why, test evidence, and benchmark
   numbers (via `benchmarks/run_all.py`) for performance-sensitive changes —
   report your hardware and methodology honestly, per the benchmark guide.
4. CI runs fmt, clippy, unit tests, MinIO integration tests, and SDK tests.
   Two maintainer approvals are needed for storage-format changes; one
   otherwise.

## Good first areas

Issues labeled `good-first-issue`. Generally friendly areas: CLI ergonomics,
SDK convenience helpers, demo improvements, doc fixes, additional filter
operators, benchmark scenarios. Areas needing design discussion first:
manifest format, compaction policy, consistency semantics, branching/GC.

## Conduct

We follow the [Contributor Covenant v2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).
Be kind; assume good faith; critique code, not people. Conduct concerns:
conduct@lagoon-db.dev.
