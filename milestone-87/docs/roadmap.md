# Lagoon Roadmap & Non-Goals

This document states honestly where Lagoon is, where it is going, and — just
as importantly — what it deliberately is **not**. Roadmap items are intent,
not commitments; ordering reflects current priorities and may change with
community input.

## Status: v1 (current)

Delivered and tested:

- Object-storage-native storage engine: WAL, immutable segments, manifest CAS
  commits, tiered compaction, GC, crash recovery (filesystem, MinIO, S3).
- Query engine: exact kNN + IVF-Flat ANN (cosine/dot/euclidean), BM25
  full-text with field weighting, sparse vectors, exact pre-filtering with the
  full filter AST, RRF + weighted hybrid fusion, multi-query, query planner.
- Stateless HTTP API server with API keys and reader/writer/admin roles;
  memory + disk LRU cache hierarchy with warming and pinning.
- Copy-on-write namespace branching with reference-safe GC.
- CLI, Python SDK, TypeScript SDK, OpenAPI spec, Docker Compose, Kubernetes
  manifests/Helm chart.
- Demos (semantic/RAG, hybrid, code-search-with-branching), benchmark suite,
  and this documentation set.

## v1.x — Hardening (next)

- **Cost-aware planner**: feed object-storage fetch counts into plan choice,
  not just selectivity estimates.
- **Scalar quantization (int8) for IVF posting lists** — ~4× smaller vector
  segments, with measured recall impact published in the benchmark results.
- **Smarter warming**: access-pattern-driven prefetch of postings blocks
  instead of whole-segment warming.
- **Conditional-write coverage** for object stores lacking compare-and-swap
  (lease-based fallback is in; needs more soak testing).
- **Backpressure & admission control** when indexing lag grows.
- Expanded chaos/recovery test matrix (fault injection on every storage op).

## v2 — Scale-out

- **Namespace sharding** for namespaces beyond single-node cache capacity
  (hash-partitioned segments, scatter-gather query execution).
- **Distributed read-your-writes** improvements: manifest change notification
  (e.g., via object-store event streams) to shrink the staleness window below
  the poll interval.
- **Multi-tenant scheduling**: per-key resource isolation beyond rate limits.
- **Incremental reindexing** on schema changes (today: full rebuild).
- **Snapshot/restore tooling** built on branching + export.

## v2+ — Exploration (no committed order)

- Product quantization / OPQ for very large vector corpora.
- Learned or adaptive `nprobe` selection targeting a recall SLO.
- Streaming change feed per namespace (consume the WAL as CDC).
- Multi-region replication (bucket-level replication + manifest fencing).
- Pluggable rerankers (cross-encoder hook executed server-side).
- GPU-accelerated scoring for batch/offline workloads.

## Non-Goals for v1

We say no to these on purpose. Some may graduate to the roadmap later; many
never will.

1. **Not a general-purpose OLTP/OLAP database.** No joins, aggregations,
   transactions across namespaces, or SQL. Lagoon stores documents and ranks
   them.
2. **No memory-resident graph ANN (HNSW etc.).** v1 commits to IVF-style
   indexes because they page well from object storage and rebuild
   deterministically. If your workload demands graph-index latency at the cost
   of RAM-residency, other tools serve that better today.
3. **No built-in embedding generation.** Lagoon never calls OpenAI, Cohere,
   or any model. Demos show pluggable provider patterns; the database stays
   vendor-neutral.
4. **No strong cross-node consistency.** Multi-node reads are bounded-stale by
   design (manifests are polled). We document this instead of pretending
   otherwise; `min_manifest_generation` exists for callers who need
   read-your-writes across nodes.
5. **No per-document ACLs or row-level security.** Authorization is
   key + role + namespace scope. Model finer-grained access with namespaces
   (branching makes per-tenant/per-workspace namespaces cheap).
6. **No client-side encryption.** Encryption at rest is delegated to provider
   SSE (see the deployment guide); in-transit encryption to your TLS proxy.
7. **No exactly-once cross-system delivery guarantees.** Idempotency keys give
   safe retries; distributed transactions with your other systems are your
   orchestration layer's job.
8. **No performance-parity claims against commercial products.** Our
   benchmark suite measures *Lagoon* honestly on disclosed hardware with a
   reproducible methodology. We publish our numbers and our methodology; we do
   not publish comparisons we haven't rigorously run, and we make no parity
   claims about Turbopuffer or any other product.
9. **No serverless control plane, billing, or hosted offering** in this
   repository. Lagoon v1 is self-hosted software.
10. **No automatic schema inference migrations.** Vector dimensions and
    metrics are immutable per namespace; changing them means a new namespace
    (exports + branching make this cheap).

## How to influence this roadmap

Open a GitHub discussion with your workload shape (corpus size, QPS,
latency/recall targets, cold-vs-warm mix). Real workload evidence moves items
up this list faster than anything else.
