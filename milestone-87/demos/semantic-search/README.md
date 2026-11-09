# Demo 1 — Semantic Document Search & RAG

This demo ingests a small bundled corpus of ~40 short encyclopedic articles,
embeds them with a pluggable embedding provider, and serves:

1. **Semantic search** — vector similarity over article bodies, optionally
   filtered by category or year.
2. **A minimal RAG pipeline** — retrieve top passages for a question, assemble
   a grounded context, and either (a) produce an extractive answer locally with
   no LLM dependency, or (b) optionally call an LLM you configure.

Everything runs offline by default: the default embedding provider is a
deterministic local feature-hashing embedder with no model downloads and no
API keys.

## Run it

```bash
# from the repository root, with the stack running (docker compose up -d)
pip install -r demos/requirements.txt

# 1. Ingest the bundled dataset (creates namespace "demo-articles")
python demos/semantic-search/ingest.py

# 2. Search
python demos/semantic-search/search.py "why do coral reefs matter"
python demos/semantic-search/search.py "how does fermentation preserve food" --filter-category food
python demos/semantic-search/search.py --interactive

# 3. Ask a question through the RAG pipeline (extractive, no LLM needed)
python demos/semantic-search/rag.py "What did the printing press change about books?"

# Optional: use an LLM for answer synthesis (requires `pip install openai` + OPENAI_API_KEY)
python demos/semantic-search/rag.py "What did the printing press change?" --llm openai
```

## Better embeddings (optional)

```bash
pip install 'sentence-transformers>=2.6'
LAGOON_EMBEDDINGS=sentence-transformers python demos/semantic-search/ingest.py
LAGOON_EMBEDDINGS=sentence-transformers python demos/semantic-search/search.py "underwater ecosystems"
```

The namespace records which provider produced its vectors; `search.py` refuses
to query with a mismatched provider so you never compare vectors from
different embedding spaces.

## Dataset

`data/articles.jsonl` — 40 original short articles written for this demo
(science, history, technology, nature, food, space, art, music, sports,
geography), each with `id`, `title`, `body`, `category`, `year`.

## What to look at

- `ingest.py` — batched upserts, schema with full-text + filterable fields,
  waiting for background indexing to catch up.
- `search.py` — vector queries with metadata filters and attribute projection.
- `rag.py` — retrieval, context assembly with source attributions, extractive
  answering, and an optional pluggable LLM hook.
