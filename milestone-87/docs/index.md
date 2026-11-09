# Lagoon Documentation

Lagoon is an open-source, object-storage-native vector + full-text search
database for AI and RAG applications. Durable state lives in S3-compatible
object storage; stateless compute nodes serve queries through SSD and memory
caches.

## Start here

- [README / Quickstart](../README.md) — install, run with Docker Compose,
  first queries in five minutes.
- [Demos](../demos/) — three runnable demos with bundled datasets:
  - [Semantic search & RAG](../demos/semantic-search/README.md)
  - [Hybrid text + vector search](../demos/hybrid-search/README.md)
  - [Code search with namespace branching](../demos/code-search/README.md)

## Reference

- [Architecture guide](architecture.md) — storage format, write path, query
  path, consistency model, and the trade-offs behind them.
- [API reference](api-reference.md) — every HTTP endpoint, request/response
  schemas, filters, and error codes.
- [Python SDK](python-sdk.md)
- [TypeScript SDK](typescript-sdk.md)

## Operations

- [Deployment guide](deployment.md) — Docker Compose, Kubernetes, MinIO and
  AWS S3 configuration, encryption-at-rest via provider SSE, threat model,
  and hardening guidance.
- [Benchmark guide](benchmark-guide.md) — how to run the suite, methodology,
  and the [results report template](../benchmarks/results/TEMPLATE.md) with
  its honest-reporting policy.

## Project

- [Contributing](contributing.md) — dev environment, test matrix, PR
  expectations.
- [Roadmap](roadmap.md) — what comes after v1.
- [Non-goals for v1](non-goals.md) — what Lagoon deliberately does not do,
  and why. Read this before adopting Lagoon for a workload.
