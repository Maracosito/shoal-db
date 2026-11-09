# Non-Goals for v1

Lagoon v1 is deliberately narrow. This document lists what Lagoon **does not**
try to do in its first major release, and why. Treating these as explicit
non-goals keeps the core small, auditable, and honest about its operational
envelope. Several items here are revisited in the [roadmap](roadmap.md) as
candidates for later releases; others are permanent non-goals.

If you need one of these capabilities today, Lagoon is probably the wrong tool
for that workload — and we would rather tell you that up front than have you
discover it in production.

---

## 1. Not a distributed consensus system

**Non-goal:** multi-writer clusters, Raft/Paxos replication, leader election,
or any coordination protocol between compute nodes.

**Why:** Lagoon's durability story is object storage, not a quorum of nodes.
A single writer per namespace (enforced by conditional writes on the manifest)
plus any number of stateless readers covers the target workloads — RAG corpora,
search indexes, embedding stores — without the operational weight of a
consensus layer. Multi-node *read* scale-out works in v1 because readers only
consume immutable objects; multi-node *write* scale-out is a roadmap item
(per-namespace writer leases), not a v1 feature.

**Consequence you should plan for:** concurrent writers to the same namespace
will see conditional-write conflicts; one of them loses and must retry. This is
documented behavior, not a bug.

## 2. Not a strongly consistent multi-node database

**Non-goal:** linearizable reads across a fleet of query nodes.

**Why:** readers cache manifests and segments. A read served by a node with a
stale cached manifest can lag the latest commit by up to the manifest refresh
interval. Single-node deployments get read-your-writes (the writer invalidates
its own caches on commit); multi-node deployments get bounded-staleness reads.
The exact guarantees are specified in the
[architecture guide](architecture.md) — we will not paper over them with
marketing language.

## 3. Not an OLTP or relational database

**Non-goal:** SQL, joins, secondary unique constraints, multi-document
transactions, foreign keys, or interactive row-level mutation at OLTP rates.

**Why:** the storage layout (append-only WAL + immutable segments + periodic
compaction) is optimized for batched ingest and read-heavy search. Single-
document patches work, but they are implemented as WAL appends and are not
cheap at high per-document mutation rates. If your workload is "update one row
ten thousand times per second," use an OLTP database and sync into Lagoon.

The only transactional unit in v1 is a **single batch write to a single
namespace** (atomic via the manifest commit). There are no cross-namespace
transactions.

## 4. Not an in-memory graph-index engine

**Non-goal:** HNSW or other graph ANN indexes that require the full graph
resident in RAM, and the single-digit-millisecond p99s they enable.

**Why:** graph indexes fight the object-storage-native design — they are
expensive to build incrementally, awkward to page from cold storage, and force
memory provisioning proportional to corpus size. Lagoon's IVF/centroid index
is chosen precisely because its posting lists are independently fetchable
objects that cache well on SSD. The honest trade: warm IVF queries land in the
low tens of milliseconds at our tested scales (see the
[benchmark guide](benchmark-guide.md)), not the sub-millisecond range that
RAM-resident graph engines advertise. We will not claim otherwise.

## 5. Not an embedding service

**Non-goal:** generating embeddings, bundling models, or depending on any
embedding vendor.

**Why:** embedding models churn monthly and pricing/licensing varies wildly.
Lagoon stores and searches vectors; producing them is your pipeline's job. The
demos ship a pluggable provider shim (`demos/common/embeddings.py`) with
hash-based, sentence-transformers, and OpenAI-compatible backends purely as a
convenience — none of them is a dependency of the server, and the server never
makes outbound network calls to embedding APIs.

## 6. Not a real-time streaming system

**Non-goal:** sub-second ingest-to-visible latency guarantees, change-data-
capture feeds, or subscription/notification APIs.

**Why:** writes are durable on WAL commit, but visibility of *indexed* (ANN /
inverted-index) search over new data depends on background indexing, which runs
on a configurable interval. Freshly written documents are still queryable via
the exact/scan paths before indexing completes, so correctness is preserved —
but if your SLA is "indexed and searchable within 200 ms of write," v1 does not
promise that.

## 7. Not a multi-tenant SaaS control plane

**Non-goal:** billing, usage metering for invoicing, per-tenant noisy-neighbor
isolation guarantees, self-serve signup, or a hosted dashboard.

**Why:** v1 ships the primitives a platform team needs — organizations,
projects, namespaces, API keys with admin/writer/reader roles, optional rate
limits and quotas, audit logs — and stops there. Building a SaaS on top of
Lagoon is a supported use case; *being* the SaaS is not.

## 8. Not a security appliance

**Non-goal:** built-in TLS termination, mTLS between components, per-document
ACLs, row-level security, or client-side encryption.

**Why:** v1's security model is API keys + roles at the namespace boundary,
deployed behind your reverse proxy/service mesh for transport security, with
encryption-at-rest delegated to the object-storage provider (SSE-S3 / SSE-KMS;
see the [deployment guide](deployment.md#encryption-at-rest)). Per-document
authorization belongs in your application layer in v1. The threat model and
hardening guidance in the deployment guide state exactly what is and is not
covered.

## 9. Not exhaustive query-language coverage

Specific query features that are **out of scope for v1**:

- Regular-expression filters (only `Eq`/`NotEq`/range/`In`/`ContainsAny`/
  prefix-style string matching are supported).
- Aggregations, facets, and group-by.
- Geo-spatial queries.
- Fuzzy/typo-tolerant full-text matching (BM25 over exact tokens only;
  stemming is per-namespace configurable, edit-distance matching is not).
- Learned/re-ranking models inside the engine. Hybrid fusion (weighted-sum and
  RRF) is the ceiling; cross-encoder re-ranking happens in your application,
  as the RAG demo shows.
- Vector quantization (PQ/SQ) for the v1 on-disk format. Vectors are stored
  as float32; the segment format reserves an encoding byte so quantization can
  arrive without a format break.

## 10. Not a benchmark-marketing project

**Non-goal:** claiming performance parity with Turbopuffer, Elasticsearch,
Qdrant, or any other system we have not benchmarked head-to-head under
published, reproducible conditions.

**Why:** Lagoon is *inspired by* the object-storage-native architecture that
Turbopuffer described publicly, but it is an independent clean-room
implementation, and we have no access to those systems' internals or
comparable test environments. All performance numbers we publish come from the
bundled benchmark suite, with hardware, dataset, and configuration fully
disclosed (see [benchmarks/results/TEMPLATE.md](../benchmarks/results/TEMPLATE.md)).
Numbers without a reproduction recipe do not get published. Pull requests that
add unverifiable comparison claims to the docs will be declined.

## 11. Other explicit exclusions

| Excluded in v1 | Status |
|---|---|
| GPU acceleration for distance computation | Permanent non-goal for the core; viable as an external plugin |
| Windows as a supported server platform | Roadmap "maybe" — clients and CLI work; the server is tested on Linux/macOS only |
| Embedded/library mode (linking the engine into your process) | Roadmap candidate; the storage crates are structured to allow it, but the supported surface is the HTTP API |
| Schema migration tooling | Documents are schemaless; attribute-index definitions can be changed and rebuilt, but there is no general migration framework |
| Cross-region replication | Use your object store's bucket replication; Lagoon does not coordinate it and does not guarantee read consistency across replicated regions |
| Plugin/extension API stability | Internal Rust APIs may break between minor versions until 1.0; only the HTTP API and storage format carry compatibility promises |

---

## How to read this document

A non-goal is not a value judgment about the feature — most of these are
excellent features in systems designed for them. It is a statement that v1
will not attempt them, so that what v1 *does* ship — durable object-storage-
native storage, IVF vector search, BM25, hybrid fusion, filters, branching,
and a tight cache hierarchy — ships solid, tested, and honestly documented.

When a non-goal graduates to a goal, it moves to the [roadmap](roadmap.md)
with a design sketch, and this document is updated in the same pull request.
