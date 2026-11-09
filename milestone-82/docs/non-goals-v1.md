# Non-Goals for v1

Being explicit about what Gannet v1 will **not** do is as important as the feature
list. These exclusions keep the v1 scope honest, shippable, and reviewable.

## Out of scope for v1

1. **Strong cross-node consistency / consensus.** v1 provides durable writes and
   read-your-writes on a single-node deployment, and *documented* bounded-staleness
   eventual consistency in multi-node read scale-out. We do not run Raft/Paxos, and
   we do not provide linearizable multi-node reads. Coordination uses object-storage
   conditional writes (compare-and-swap on manifests) only.

2. **Multi-document ACID transactions.** A write batch to a single namespace is
   atomic and durable; there are no transactions spanning batches or namespaces.

3. **SQL or a general query language.** Queries are structured JSON (vector,
   full-text, hybrid, filters). No joins, aggregations beyond counts, or analytics.

4. **Graph-based in-memory ANN (HNSW) as the primary vector index.** v1 uses an
   IVF/centroid-partitioned index because it maps cleanly onto object-storage-backed,
   partially-hydrated retrieval. HNSW may come later as an opt-in for fully-warm
   namespaces.

5. **Built-in embedding generation.** Gannet stores and searches vectors; it never
   calls an embedding vendor. Demos ship with a *pluggable* embedding script
   (OpenAI-compatible HTTP, local sentence-transformers, or precomputed files), but
   the database has zero vendor dependencies.

6. **Sharding a single namespace across nodes.** A namespace is served by one node
   at a time for writes (object-storage CAS provides fencing). Very large namespaces
   (>~50M documents) are out of v1's performance envelope.

7. **Multi-region replication.** Durability is whatever the underlying object store
   provides (S3: 11 nines, regional). Cross-region DR = bucket replication, which is
   the operator's responsibility and documented, not implemented.

8. **Fine-grained per-document ACLs.** v1 security is API keys with three roles
   (admin / writer / reader) scoped to organization or project. Row-level security
   is a non-goal.

9. **Online schema enforcement.** Documents are semi-structured; attribute types are
   inferred and recorded per-namespace. v1 rejects type-conflicting writes but does
   not support user-defined schemas, required fields, or migrations.

10. **A hosted control plane, billing, or web console.** Gannet v1 is a database you
    run yourself: server + worker + object storage. A minimal HTML status page may
    exist; a full UI does not.

11. **Vector quantization beyond float32 + optional scalar (int8) quantization.**
    Product quantization, binary embeddings, and Matryoshka truncation are post-v1.

12. **Geospatial, regex, and full Unicode collation in filters.** v1 string matching
    is exact equality, `in`, `contains_any` (for array attributes), and prefix match.

13. **Tunable per-request consistency levels.** There is exactly one consistency
    story per deployment topology; it is documented, not configurable.

## Deliberate tradeoffs accepted for v1

- **Write latency floor ≈ one object-storage PUT** (~20–80 ms on S3, ~1–5 ms on
  MinIO/local). We batch concurrent writers into shared WAL flushes rather than
  pretending object storage is a local disk.
- **Cold-query latency is seconds, not milliseconds**, for large namespaces. That is
  the price of $0.02/GB-month storage; the cache hierarchy and warm/pin APIs exist
  precisely to manage it.
- **Garbage collection is reachability-based and delayed.** Deleted/compacted
  objects linger for a configurable grace window (default 24 h) so that in-flight
  readers and branches are never broken. Storage is slightly over-provisioned in
  exchange for safety.
