#!/usr/bin/env python3
"""Compare BM25, vector, and hybrid search side by side on the product catalog.

Usage:
    python demos/hybrid-search/search.py                       # built-in example queries
    python demos/hybrid-search/search.py "warm winter jacket"  # your own query
    python demos/hybrid-search/search.py "running shoes" --category footwear --max-price 150
    python demos/hybrid-search/search.py --interactive
"""
from __future__ import annotations

import argparse
import pathlib
import sys
from typing import List, Optional

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from common.embeddings import get_provider
from common.lagoon_client import LagoonClient

EXAMPLE_QUERIES = [
    "waterproof shoes for muddy trail running",
    "quiet espresso machine for a small kitchen",
    "lightweight laptop for travel",
    "warm jacket for winter camping",
    "BurrMaster grinder",  # exact rare token: BM25 should nail this
]

FIELD_BOOSTS = {"title": 2.0, "description": 1.0}


def build_filter(category: Optional[str], max_price: Optional[float], in_stock: bool) -> Optional[list]:
    clauses: List[list] = []
    if category:
        clauses.append(["category", "Eq", category])
    if max_price is not None:
        clauses.append(["price", "Lte", max_price])
    if in_stock:
        clauses.append(["in_stock", "Eq", True])
    if not clauses:
        return None
    return clauses[0] if len(clauses) == 1 else ["And", clauses]


def fmt_hits(hits: List[dict], width: int = 38) -> List[str]:
    lines = []
    for hit in hits:
        attrs = hit.get("attributes", {})
        title = str(attrs.get("title", hit["id"]))[: width - 8]
        lines.append(f"{hit['score']:6.3f}  {title}")
    return lines


def run_comparison(client: LagoonClient, provider, namespace: str, query: str,
                   flt: Optional[list], top_k: int) -> None:
    vector = provider.embed([query])[0]
    common = dict(top_k=top_k, filter=flt, include_attributes=["title", "price", "category"])

    columns = {
        "BM25": client.query(namespace, text=query, mode="bm25", fields=FIELD_BOOSTS, **common),
        "Vector": client.query(namespace, vector=vector, mode="vector", **common),
        "Hybrid (RRF)": client.query(
            namespace, text=query, vector=vector, mode="hybrid",
            fusion={"method": "rrf", "k": 60}, fields=FIELD_BOOSTS, **common,
        ),
        "Hybrid (weighted 0.6/0.4)": client.query(
            namespace, text=query, vector=vector, mode="hybrid",
            fusion={"method": "weighted", "vector_weight": 0.6, "text_weight": 0.4},
            fields=FIELD_BOOSTS, **common,
        ),
    }

    width = 40
    print("=" * (width * 2 + 4))
    print(f"query: {query!r}" + (f"   filter: {flt}" if flt else ""))
    print("=" * (width * 2 + 4))
    names = list(columns)
    for row_pair in ((names[0], names[1]), (names[2], names[3])):
        left, right = row_pair
        left_lines, right_lines = fmt_hits(columns[left]), fmt_hits(columns[right])
        print(f"{left:<{width}}| {right}")
        print(f"{'-' * (width - 2):<{width}}| {'-' * (width - 2)}")
        for i in range(max(len(left_lines), len(right_lines))):
            l = left_lines[i] if i < len(left_lines) else ""
            r = right_lines[i] if i < len(right_lines) else ""
            print(f"{l:<{width}}| {r}")
        print()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("query", nargs="?", default=None)
    ap.add_argument("--namespace", default="demo-products")
    ap.add_argument("--provider", default=None)
    ap.add_argument("--top-k", type=int, default=5)
    ap.add_argument("--category", default=None)
    ap.add_argument("--max-price", type=float, default=None)
    ap.add_argument("--in-stock", action="store_true", help="only show in-stock items")
    ap.add_argument("--interactive", action="store_true")
    args = ap.parse_args()

    client = LagoonClient()
    provider = get_provider(args.provider)
    flt = build_filter(args.category, args.max_price, args.in_stock)

    if args.interactive:
        print("hybrid search — type a query, or 'quit' to exit")
        while True:
            try:
                query = input("query> ").strip()
            except (EOFError, KeyboardInterrupt):
                print()
                break
            if not query or query.lower() in {"quit", "exit"}:
                break
            run_comparison(client, provider, args.namespace, query, flt, args.top_k)
        return 0

    queries = [args.query] if args.query else EXAMPLE_QUERIES
    for query in queries:
        run_comparison(client, provider, args.namespace, query, flt, args.top_k)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
