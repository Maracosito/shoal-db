# Shoal end-to-end test suites

These suites drive a **live Shoal server** through the official Python and
TypeScript SDKs. They are the final contract check: if these pass, a developer
can adopt the SDKs against a real deployment.

Both suites are **opt-in**: they skip themselves entirely unless
`SHOAL_E2E_URL` is set, so running `pytest`/`vitest` in normal unit-test CI
stays hermetic.

## Environment variables

| Variable            | Meaning                              | Default        |
|---------------------|--------------------------------------|----------------|
| `SHOAL_E2E_URL`     | Base URL of the running API server   | *(unset → skip all tests)* |
| `SHOAL_E2E_API_KEY` | API key with admin privileges        | `dev-root-key` |

## Running locally

Start the full stack (API, worker, MinIO) with Docker Compose:

```bash
SHOAL_API_KEY=dev-root-key docker compose -f deploy/docker/docker-compose.yml up -d --build
export SHOAL_E2E_URL=http://localhost:8080
export SHOAL_E2E_API_KEY=dev-root-key
```

### Python suite

```bash
cd e2e/python
python -m venv .venv && . .venv/bin/activate
pip install -r requirements.txt     # installs the SDK from ../../sdks/python in editable mode
python -m pytest -v
```

### TypeScript suite

```bash
cd e2e/typescript
npm install
npm test
```

The TypeScript suite imports the SDK **source** directly (via a `@sdk` path
alias into `../../sdks/typescript/src`), so no separate SDK build step is
required.

## What is covered

- Namespace lifecycle: create, list, metadata, delete, missing-namespace errors.
- Ingest: batched upserts, idempotent re-upsert, patch, delete by ID, delete by filter.
- Query: dense vector search, BM25 full-text, hybrid fusion, `top_k`,
  attribute projection, and every filter operator (`Eq`, `Gt`, `Gte`, `Lt`,
  `Lte`, `In`, `ContainsAny`, `And`, `Or`, `Not`).
- Export: full round-trip of documents and attribute types.
- Branching: copy-on-write isolation in both directions, multi-level branches,
  branch deletion not affecting the source, namespace copies.
- Cache: warm/pin endpoints.
- Both sync and async Python clients.

Each test uses a uniquely named namespace (`e2e-py-*` / `e2e-ts-*`) and cleans
up after itself, so the suites are safe to run repeatedly against a shared dev
deployment.

In CI these suites run in the `E2E` workflow
(`.github/workflows/e2e.yml`) against the Docker Compose stack.
