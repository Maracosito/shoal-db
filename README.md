# Shoal DB

**Open-source, object-storage-native vector and full-text (BM25) hybrid search database written in Rust**, inspired by Turbopuffer.

Shoal combines **ANN vector search**, **BM25 lexical search**, **Reciprocal Rank Fusion (RRF)**, **indexed attribute filtering**, and **O(1) copy-on-write namespace branching** on top of S3-compatible object storage (MinIO / AWS S3) or local filesystem storage (`file://`), backed by a two-tier memory + NVMe cache.

---

## Architecture

```text
Client / MCP Server (elias-context, Python/TS SDKs, CLI)
   │
   ▼
Shoal AI Embed Proxy (:8090)  ──► Gemini Embeddings (768d, auto-fallback + graceful BM25 degradation)
   │
   ▼
Shoal Server (:8080, Rust)    ──► Two-Tier Cache (Memory + Local Disk)
   │                                 ├─► shoal-query (BM25 + HNSW/Cosine Vector + RRF Fusion)
   │                                 ├─► reef-engine (WAL, Compaction, O(1) Namespace Branching)
   ▼                                 └─► gannet-format (Immutable Columnar/Index Segments)
Object Storage (MinIO S3 :9000 or Local Filesystem `file://`)
```

---

## Repository Layout

This repository contains both the **unified production workspace (`shoal-build/`)** and the **6 architectural milestones (`milestone-82` – `milestone-87`)** showing the progressive construction of each storage and query layer:

| Directory | Description |
|---|---|
| **`shoal-build/`** | **Unified production workspace** (`Cargo.toml`, all Rust crates, CLI, Python/TS SDKs, Docker/K8s/Helm deployment, and `embed-proxy.js` v1.2) |
| `milestone-82/` | **Gannet**: Immutable segment format, object-store abstraction (`file://` & `s3://`), checksums, and reader/writer primitives |
| `milestone-83/` | **Reef**: Write-Ahead Log (WAL), background indexer, segment compaction, and O(1) namespace branching |
| `milestone-84/` | **Shoal Query**: BM25 inverted index, vector similarity engine, attribute filter planner, and Reciprocal Rank Fusion (RRF) |
| `milestone-85/` | **Shoal Server**: Multi-tenant HTTP API (`/v1/orgs/.../projects/.../namespaces/...`), two-tier cache, RBAC auth, quotas, rate limiting, and audit logs |
| `milestone-86/` | **Ecosystem & Deploy**: Python SDK, TypeScript SDK, `shoal` CLI, Docker Compose stack, Helm chart, and Kubernetes manifests |
| `milestone-87/` | **Reference Applications & Benchmarks**: Semantic Search, RAG with citations, Multi-tenant SaaS isolation, and Observability demos |

---

## Quick Start (Local Docker Stack)

1. Configure your environment file in `shoal-build/deploy/docker/.env`:
   ```bash
   cp shoal-build/deploy/docker/.env.example shoal-build/deploy/docker/.env
   # Set GEMINI_API_KEY=<your-key> for automatic 768d embeddings on port 8090
   ```

2. Start the stack:
   ```bash
   docker compose -f shoal-build/deploy/docker/docker-compose.yml up -d --build
   ```

3. Endpoints:
   - **AI Hybrid Proxy (auto-embeds text + BM25 + RRF)**: `http://localhost:8090` (`Authorization: Bearer dev-root-key`)
   - **Shoal Core Rust API**: `http://localhost:8080`
   - **Demo Web UI**: `http://localhost:8000`
   - **MinIO Console**: `http://localhost:9001`

---

## Hybrid Query Example (via Port 8090)

```bash
# Upsert document (vector is automatically generated in 768d by the AI proxy)
curl -X POST http://localhost:8090/v1/orgs/default/projects/default/namespaces/my-notes/documents \
  -H "Authorization: Bearer dev-root-key" \
  -H "Content-Type: application/json" \
  -d '{
    "documents": [{
      "id": "doc-1",
      "text": { "resumen": "Hybrid RRF combines BM25 exact keyword matching with semantic vectors" },
      "attributes": { "tipo": "session", "proyecto": "shoal" }
    }]
  }'

# Query (automatically runs BM25 + 768d vector similarity + RRF fusion)
curl -X POST http://localhost:8090/v1/orgs/default/projects/default/namespaces/my-notes/query \
  -H "Authorization: Bearer dev-root-key" \
  -H "Content-Type: application/json" \
  -d '{
    "text": { "query": "semantic keyword fusion" },
    "top_k": 5
  }'
```
