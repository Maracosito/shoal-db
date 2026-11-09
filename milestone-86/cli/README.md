# lakefin-cli

Command-line interface for [Lakefin](../README.md), the object-storage-native
vector + full-text search database.

## Install

```sh
cargo install --path cli
```

## Quickstart

```sh
# Start the local dev stack (API server, indexer worker, MinIO)
lakefin dev up

# Create a namespace and load data
lakefin ns create articles --dims 384 --metric cosine
lakefin load data/articles.jsonl -n articles

# Query: full-text, vector, hybrid, filtered
lakefin query -n articles --text "object storage caching"
lakefin query -n articles --vector-file query-embedding.json -k 5
lakefin query -n articles --text "caching" --vector-file q.json --fusion rrf
lakefin query -n articles --text "caching" --where 'year>=2020' --where 'lang=en'

# Export, warm caches, benchmark
lakefin export -n articles -o backup.jsonl --include-vectors
lakefin ns warm articles --pin
lakefin bench ingest -n bench --docs 50000 --dims 128 --create --with-text
lakefin bench query  -n bench --mode hybrid --queries 500 --warm
```

Configuration:

| Flag        | Env variable      | Default                 |
|-------------|-------------------|-------------------------|
| `--url`     | `LAKEFIN_URL`     | `http://localhost:8080` |
| `--api-key` | `LAKEFIN_API_KEY` | (none)                  |
| `--json`    |                   | human-readable output   |

Full reference: [docs/cli.md](../docs/cli.md).
