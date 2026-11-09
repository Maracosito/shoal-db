# Hybrid Search Demo — Product Catalog

This demo searches a 40-item product catalog three ways and lets you compare
the results side by side:

1. **BM25 full-text** — classic keyword relevance over `title` and
   `description`, with a 2× boost on titles.
2. **Dense vector** — nearest-neighbour search over embeddings of the
   product text.
3. **Hybrid** — both at once, fused on the server with either
   **reciprocal-rank fusion (RRF)** or **weighted score fusion**.

It also demonstrates metadata filters (`category`, `price`, `in_stock`)
applied uniformly across all three modes.

## Why hybrid?

- BM25 wins on exact terms the embedding may dilute: model numbers, brand
  names, rare tokens ("BurrMaster", "55L").
- Vectors win on paraphrase: "quiet espresso machine for a small kitchen"
  should match a product described as "low-noise compact espresso maker"
  even with little word overlap.
- RRF fuses the two rankings without comparing incompatible score scales:
  `score(d) = Σ 1 / (k + rank_i(d))` with `k = 60` by default. Weighted
  fusion instead min-max-normalises each list's scores and blends them
  (`0.6 * vector + 0.4 * bm25` by default) — more tunable, more brittle.

## Run it

From the repository root, with the dev stack up (`docker compose up -d` or
`lagoon dev up`):

```bash
pip install -r demos/requirements.txt

# 1. Ingest the catalog (creates namespace `demo-products`)
python demos/hybrid-search/ingest.py

# 2. Compare modes on the built-in example queries
python demos/hybrid-search/search.py

# 3. Ask your own question
python demos/hybrid-search/search.py "warm jacket for winter camping"

# 4. Add filters
python demos/hybrid-search/search.py "running shoes" --category footwear --max-price 150

# 5. Interactive loop
python demos/hybrid-search/search.py --interactive
```

## Embeddings

The default `hash` provider is offline and deterministic but only lexical —
it makes the demo runnable anywhere, with the honest caveat that the
"vector" column will look keyword-ish. For genuinely semantic vectors:

```bash
export LAGOON_EMBED_PROVIDER=openai   # needs OPENAI_API_KEY
# or
pip install sentence-transformers
export LAGOON_EMBED_PROVIDER=st
```

Re-run `ingest.py` after switching providers (the namespace is recreated
with the provider's dimensionality).
