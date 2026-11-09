#!/usr/bin/env python3
"""Ingest the bundled article corpus into LagoonDB for the semantic-search demo.

Usage:
    python demos/semantic-search/ingest.py
    LAGOON_EMBEDDINGS=sentence-transformers python demos/semantic-search/ingest.py
    python demos/semantic-search/ingest.py --namespace my-articles --recreate
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

# Allow running from the repo root or from this directory.
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from demos.common.embeddings import get_provider  # noqa: E402
from demos.common.lagoon_client import LagoonClient  # noqa: E402

DATA_FILE = Path(__file__).resolve().parent / "data" / "articles.jsonl"
DEFAULT_NAMESPACE = "demo-articles"


def load_articles() -> list[dict]:
    articles = []
    with DATA_FILE.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                articles.append(json.loads(line))
    return articles


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--namespace", default=DEFAULT_NAMESPACE)
    parser.add_argument(
        "--provider",
        default=None,
        help="embedding provider: hash | sentence-transformers | openai "
        "(default: $LAGOON_EMBEDDINGS or 'hash')",
    )
    parser.add_argument(
        "--recreate",
        action="store_true",
        help="drop and recreate the namespace before ingesting",
    )
    args = parser.parse_args()

    provider = get_provider(args.provider)
    client = LagoonClient()
    articles = load_articles()
    print(f"Loaded {len(articles)} articles from {DATA_FILE.name}")
    print(f"Embedding provider: {provider.name} ({provider.dims} dims)")

    if args.recreate:
        client.delete_namespace(args.namespace)

    client.create_namespace(
        args.namespace,
        schema={
            "vector": {"dims": provider.dims, "metric": "cosine"},
            "full_text": ["title", "body"],
            "filterable": ["category", "year"],
            # Record the embedding space so queries can verify compatibility.
            "annotations": {"embedding_provider": provider.name},
        },
    )

    # Embed title + body together so titles contribute to the vector.
    texts = [f"{a['title']}. {a['body']}" for a in articles]
    t0 = time.time()
    vectors = provider.embed(texts)
    print(f"Embedded {len(texts)} documents in {time.time() - t0:.2f}s")

    documents = [
        {
            "id": article["id"],
            "vector": vector,
            "attributes": {
                "title": article["title"],
                "body": article["body"],
                "category": article["category"],
                "year": article["year"],
            },
        }
        for article, vector in zip(articles, vectors)
    ]

    t0 = time.time()
    written = client.upsert(args.namespace, documents, batch_size=100)
    print(f"Upserted {written} documents in {time.time() - t0:.2f}s (WAL durable)")

    ns = client.wait_for_indexing(args.namespace)
    stats = ns.get("stats", {})
    print(
        "Background indexing complete: "
        f"{stats.get('document_count', written)} docs, "
        f"{stats.get('segment_count', '?')} segments"
    )
    print(f"\nReady. Try:\n  python demos/semantic-search/search.py "
          f'"why do coral reefs matter" --namespace {args.namespace}')


if __name__ == "__main__":
    main()
