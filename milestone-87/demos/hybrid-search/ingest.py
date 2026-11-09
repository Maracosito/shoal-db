#!/usr/bin/env python3
"""Ingest the bundled product catalog into the `demo-products` namespace.

Usage:
    python demos/hybrid-search/ingest.py [--namespace demo-products] [--provider hash|openai|st]
"""
from __future__ import annotations

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from common.embeddings import get_provider
from common.lagoon_client import LagoonClient, iter_jsonl

DATA_PATH = pathlib.Path(__file__).resolve().parent / "data" / "products.jsonl"


def embed_text_for(product: dict) -> str:
    tags = " ".join(product.get("tags", []))
    return f"{product['title']}. {product['description']} {tags}".strip()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--namespace", default="demo-products")
    ap.add_argument("--provider", default=None, help="embedding provider: hash | openai | st")
    ap.add_argument("--url", default=None, help="Lagoon base URL (default: LAGOON_URL env)")
    args = ap.parse_args()

    provider = get_provider(args.provider)
    client = LagoonClient(base_url=args.url)

    products = list(iter_jsonl(str(DATA_PATH)))
    print(f"embedding {len(products)} products with provider={provider.name} ...")
    vectors = provider.embed([embed_text_for(p) for p in products])

    documents = []
    for product, vector in zip(products, vectors):
        doc = dict(product)
        doc["vector"] = vector
        documents.append(doc)

    print(f"recreating namespace {args.namespace!r} (dims={provider.dims}, metric=cosine)")
    client.reset_namespace(
        args.namespace,
        dims=provider.dims,
        metric="cosine",
        text_fields=["title", "description"],
    )
    count = client.upsert(args.namespace, documents)
    print(f"upserted {count} documents into {args.namespace!r}")
    print("done. try:  python demos/hybrid-search/search.py")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
