# LagoonDB

**LagoonDB** is an open-source, object-storage-native search database for AI and RAG
applications. Durable state lives entirely in S3-compatible object storage; stateless
compute nodes serve queries through a local NVMe/memory cache hierarchy. It supports
dense vector search (exact kNN and IVF-style ANN), BM25 full-text search, hybrid
fusion, rich metadata filters, and copy-on-write namespace branching.

> LagoonDB is an independent, clean-room project. It is inspired by the public
> product category of object-storage-backed search engines (e.g. Turbopuffer,
> Elasticsearch-style systems) but shares no code, branding, or documentation with
> any of them, and makes no performance-parity claims about them.

---

## Highlights

- **Object storage is the source of truth.** Every write is durably committed to a
  write-ahead log on S3/MinIO (or local filesystem in dev mode) before it is
  acknowledged. Compute nodes can be killed and replaced at any time.
- **Cheap cold, fast warm.** Cold namespaces cost only object-storage cents. Warm
  queries are served from local SSD and memory caches with configurable LRU
  eviction, cache warming, and namespace pinning.
- **Vector + full-text + hybrid.** Cosine / dot / Euclidean dense search with exact
  and IVF-ANN execution, BM25 with field boosting, weighted-score and
  reciprocal-rank fusion, and a planner that picks the execution path.
- **Filters that actually prune.** `Eq/NotEq/Gt/Gte/Lt/Lte/In/ContainsAny` plus
  `And/Or/Not`, backed by per-segment attribute indexes.
- **Namespace branching.** Fork a namespace in milliseconds via copy-on-write
  references to immutable segments. Branches are fully isolated for reads and writes.
- **No embedding vendor lock-in.** LagoonDB stores and searches vectors; you bring
  embeddings from any provider. The bundled demos include a pluggable embedding
  layer (offline deterministic hashing by default, with optional
  sentence-transformers or OpenAI providers).

## Quickstart (5 minutes)

### 1. Start the stack

```bash
git clone https://github.com/lagoondb/lagoondb
cd lagoondb
docker compose up -d        # API server + indexer worker + MinIO
```

The API listens on `http://localhost:8800`. The development API key is
`lagoon-dev-key` (override with `LAGOON_API_KEY` in `docker-compose.yml`).

Verify it is healthy:

```bash
curl -s http://localhost:8800/healthz
# {"status":"ok","version":"0.6.0","storage":"s3://lagoon-dev"}
```

### 2. Create a namespace

```bash
curl -s -X POST http://localhost:8800/v1/namespaces \
  -H "Authorization: Bearer lagoon-dev-key" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "quickstart",
    "schema": {
      "vector":    {"dims": 4, "metric": "cosine"},
      "full_text": ["title", "body"],
      "filterable": ["category", "year"]
    }
  }'
```

### 3. Upsert documents

```bash
curl -s -X POST http://localhost:8800/v1/namespaces/quickstart/documents \
  -H "Authorization: Bearer lagoon-dev-key" \
  -H "Content-Type: application/json" \
  -d '{
    "documents": [
      {"id": "1", "vector": [0.1, 0.9, 0.0, 0.0],
       "attributes": {"title": "Coral reefs", "body": "Reefs host enormous biodiversity.",
                      "category": "nature", "year": 2021}},
      {"id": "2", "vector": [0.9, 0.1, 0.0, 0.0],
       "attributes": {"title": "Rust ownership", "body": "Ownership makes memory safety tractable.",
                      "category": "tech", "year": 2023}}
    ]
  }'
```

Writes are acknowledged only after the WAL entry is durable in object storage.

### 4. Query (vector, text, hybrid, filtered)

```bash
# Hybrid: dense vector + BM25, fused with reciprocal-rank fusion
curl -s -X POST http://localhost:8800/v1/namespaces/quickstart/query \
  -H "Authorization: Bearer lagoon-dev-key" \
  -H "Content-Type: application/json" \
  -d '{
    "top_k": 5,
    "vector": {"values": [0.1, 0.85, 0.05, 0.0], "mode": "auto"},
    "text":   {"query": "biodiversity reefs", "fields": {"title": 2.0, "body": 1.0}},
    "fusion": {"method": "rrf"},
    "filter": {"op": "Eq", "field": "category", "value": "nature"},
    "include_attributes": ["title", "category"]
  }'
```

### 5. Branch the namespace

```bash
curl -s -X POST "http://localhost:8800/v1/namespaces/quickstart:branch" \
  -H "Authorization: Bearer lagoon-dev-key" \
  -H "Content-Type: application/json" \
  -d '{"target": "quickstart-experiment"}'
```

The branch is created in milliseconds by referencing the source's immutable
segments; subsequent writes to either side are isolated.

## Run the demos

Three runnable demos with small bundled datasets live in [`demos/`](demos/):

| Demo | What it shows |
|---|---|
| [`demos/semantic-search`](demos/semantic-search/) | Semantic document search + a minimal RAG pipeline |
| [`demos/hybrid-search`](demos/hybrid-search/) | Hybrid BM25 + vector product search with filters and fusion tuning |
| [`demos/code-search`](demos/code-search/) | Code search across workspaces modeled with namespace branching |

```bash
pip install -r demos/requirements.txt
python demos/semantic-search/ingest.py
python demos/semantic-search/search.py "why do coral reefs matter"
```

All demos run fully offline by default using a deterministic local embedding
provider; set `LAGOON_EMBEDDINGS=sentence-transformers` or
`LAGOON_EMBEDDINGS=openai` for higher-quality embeddings (see
[`demos/common/embeddings.py`](demos/common/embeddings.py)).

## Client SDKs

```bash
pip install lagoon-client          # Python
npm install @lagoondb/client      # TypeScript / JavaScript
```

See [docs/python-sdk.md](docs/python-sdk.md) and [docs/typescript-sdk.md](docs/typescript-sdk.md).

## Documentation

| Doc | Contents |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Storage format, write/query paths, consistency model, tradeoffs |
| [docs/api-reference.md](docs/api-reference.md) | Full HTTP API reference |
| [docs/python-sdk.md](docs/python-sdk.md) | Python SDK guide |
| [docs/typescript-sdk.md](docs/typescript-sdk.md) | TypeScript SDK guide |
| [docs/deployment.md](docs/deployment.md) | Production deployment, S3 SSE encryption-at-rest, hardening |
| [docs/benchmarks.md](docs/benchmarks.md) | Benchmark suite, methodology, honest results |
| [docs/contributing.md](docs/contributing.md) | Contributor guide |
| [docs/roadmap.md](docs/roadmap.md) | Roadmap and explicit v1 non-goals |

## Consistency model (summary)

- **Single-node:** durable writes (acknowledged after the WAL object is committed)
  and read-your-writes. Queries merge the indexed segments with the unindexed WAL
  tail, so freshly written documents are immediately visible.
- **Multi-node:** writers are strongly durable; read replicas follow the manifest
  with bounded staleness (default manifest poll interval 250 ms). See
  [docs/architecture.md](docs/architecture.md#consistency) before relying on this.

## License

Apache-2.0. See [LICENSE](LICENSE).
