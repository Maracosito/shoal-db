# Shoal CLI guide

The `shoal` command-line tool is the fastest way to work with a Shoal
deployment: spin up a local stack, create namespaces, load data, run queries,
export, warm caches, and benchmark — all without writing code.

> Flag names below match the CLI as shipped. `shoal <command> --help` always
> prints the authoritative list for your installed version.

## Installation

Build from source (requires a recent stable Rust toolchain):

```bash
cargo install --path cli
shoal --version
```

Or run it straight from the repo during development:

```bash
cargo run -p shoal-cli -- --help
```

## Connection configuration

Every command that talks to a server resolves its connection in this order:

1. Command-line flags: `--url` and `--api-key`
2. Environment variables: `SHOAL_URL` and `SHOAL_API_KEY`
3. Defaults: `http://localhost:8080` and no API key

```bash
export SHOAL_URL=http://localhost:8080
export SHOAL_API_KEY=dev-root-key
```

The API key is never printed in logs or error output.

### Output formats

Most commands accept `--output table|json|jsonl` (default `table`). `json` and
`jsonl` are intended for piping into `jq` or other tooling.

## `shoal dev` — local development stack

Wraps the Docker Compose stack in `deploy/docker/docker-compose.yml`
(API server, background worker, MinIO).

```bash
shoal dev up        # build and start the stack in the background
shoal dev status    # show container state and the API health check
shoal dev logs      # tail service logs
shoal dev down      # stop the stack (add --volumes to also wipe data)
```

After `shoal dev up`, the API listens on `http://localhost:8080` and the
MinIO console on `http://localhost:9001`.

## `shoal ns` — namespace management

```bash
shoal ns create docs                  # create a namespace
shoal ns list                         # list namespaces
shoal ns info docs                    # metadata: doc count, segments, size
shoal ns delete docs                  # delete a namespace
shoal ns copy docs docs-backup        # full independent copy
shoal ns branch docs docs-experiment  # copy-on-write branch (instant)
shoal ns warm docs                    # preload segments into local caches
shoal ns pin docs                     # keep this namespace warm
shoal ns unpin docs
```

Branches are created in O(1) by referencing the source's immutable segments;
writes to either side never affect the other. Deleting a branch never deletes
shared data still referenced by the source. See `docs/architecture.md` for the
copy-on-write details.

## `shoal load` — ingest JSON, JSONL, or CSV

```bash
# JSONL: one document per line with id / vector / attributes
shoal load --namespace docs data/articles.jsonl

# JSON: a top-level array of documents
shoal load --namespace docs data/articles.json

# CSV: header row becomes attribute names
shoal load --namespace docs data/articles.csv \
  --id-field id \
  --vector-field embedding \
  --batch-size 500
```

Behaviour:

- The format is inferred from the file extension; override with `--format`.
- `--id-field` names the column/key used as the document ID (default `id`).
- `--vector-field` names the dense-vector column/key (default `vector`).
  In CSV files the vector cell must be a JSON array, e.g. `"[0.1,0.2]"`.
- All remaining fields become metadata attributes.
- Documents are sent in batches (`--batch-size`, default 500) and the loader
  retries transient failures, so large files load reliably.
- Loading is an upsert: re-running the same file is idempotent.

## `shoal query` — run searches

```bash
# Full-text (BM25)
shoal query --namespace docs --text "coral reefs" --top-k 5

# Vector search with an inline vector
shoal query --namespace docs --vector "[0.1, 0.2, 0.3, 0.4]" --top-k 5

# Vector from a file (JSON array)
shoal query --namespace docs --vector-file query-embedding.json

# Hybrid: both signals, reciprocal-rank fusion
shoal query --namespace docs \
  --text "coral reefs" \
  --vector-file query-embedding.json \
  --fusion rrf --top-k 10

# Metadata filters (JSON filter expression)
shoal query --namespace docs --text "reef" \
  --filter '{"and":[{"eq":["genre","nature"]},{"gte":["year",2020]}]}'

# Return only selected attributes
shoal query --namespace docs --text "reef" --include title,url
```

The filter JSON grammar is the same one the HTTP API accepts; see
`docs/api.md` for the full operator reference (`eq`, `gt`, `gte`, `lt`,
`lte`, `in`, `contains_any`, `and`, `or`, `not`).

## `shoal export` — dump a namespace

```bash
shoal export --namespace docs --out docs-export.jsonl
shoal export --namespace docs            # stream JSONL to stdout
```

Exports stream every document (ID, vector, attributes) as JSONL, suitable for
re-loading with `shoal load` — including into another deployment.

## `shoal bench` — benchmarking

```bash
# Ingest throughput with synthetic documents
shoal bench ingest --namespace bench --docs 50000 --dim 384 --batch-size 500

# Query latency (reports p50/p90/p99 and throughput)
shoal bench query --namespace bench --queries 1000 --top-k 10 --concurrency 8

# Cold vs warm comparison: evict, measure, warm, measure again
shoal bench query --namespace bench --queries 200 --cold-warm
```

Benchmark output includes latency percentiles, throughput, and (when the
server exposes them) cache hit rates, so you can quantify the cold/warm gap
documented in `docs/benchmarks.md`.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success |
| 1    | Generic/usage error |
| 2    | Server returned an API error (message printed to stderr) |
| 3    | Connection failure (server unreachable) |

## Recipes

**Load a corpus and search it in under a minute:**

```bash
shoal dev up
shoal ns create articles
shoal load --namespace articles examples/compose-app/data/articles.jsonl
shoal query --namespace articles --text "search ranking" --top-k 5
```

**Experiment safely on production data:**

```bash
shoal ns branch articles articles-experiment
shoal load --namespace articles-experiment new-batch.jsonl
shoal query --namespace articles-experiment --text "..." --top-k 5
shoal ns delete articles-experiment    # source remains untouched
```

**Warm a namespace before a traffic spike:**

```bash
shoal ns warm articles && shoal ns pin articles
```

## Troubleshooting

- **`connection refused`** — the server isn't running; try `shoal dev status`.
- **`401 unauthorized`** — `SHOAL_API_KEY` is missing or wrong.
- **Slow first query** — that's a cold read from object storage; run
  `shoal ns warm <name>` or compare with `shoal bench query --cold-warm`.
