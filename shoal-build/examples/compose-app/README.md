# Shoal example app: article search

A minimal web app (Python standard library + the Shoal Python SDK, no web
framework) demonstrating vector, full-text, and hybrid search over a small
bundled article dataset.

On startup it:

1. Waits for the Shoal API to be healthy.
2. Creates the `demo-articles` namespace (64-dim, cosine) if it does not exist.
3. Upserts the 24 sample articles from `data/articles.jsonl`, embedding each
   with a deterministic, dependency-free feature-hashing embedding
   (see `embed.py`). This keeps the demo offline and vendor-free; swap in a
   real embedding provider by replacing `embed.embed`.
4. Serves a single-page search UI on port 8000 with a `mode` switch:
   `vector`, `text` (BM25), or `hybrid` (reciprocal rank fusion).

## Run via Docker Compose (recommended)

From the repository root:

```sh
docker compose -f deploy/docker/docker-compose.yml up --build
# then open http://localhost:8000
```

## Run directly

Requires a running Shoal API:

```sh
pip install ./sdks/python
SHOAL_URL=http://localhost:8080 SHOAL_API_KEY=dev-root-key python examples/compose-app/app.py
```

## Endpoints

| Method | Path | Description |
|---|---|---|
| GET | `/` | Search UI |
| GET | `/api/search?q=...&mode=hybrid\|vector\|text` | JSON search results |
| GET | `/healthz` | App health |
