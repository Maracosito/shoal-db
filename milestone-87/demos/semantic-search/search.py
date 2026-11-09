#!/usr/bin/env python3
"""Semantic search over the demo article corpus.

Usage:
    python demos/semantic-search/search.py "why do coral reefs matter"
    python demos/semantic-search/search.py "preserving food without a fridge" --filter-category food
    python demos/semantic-search/search.py "exploration" --min-year 2018 --top-k 3
    python demos/semantic-search/search.py --interactive
"""

from __future__ import annotations

import argparse
import sys
import textwrap
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from demos.common.embeddings import get_provider  # noqa: E402
from demos.common.lagoon_client import LagoonClient  # noqa: E402

DEFAULT_NAMESPACE = "demo-articles"


def build_filter(args) -> dict | None:
    clauses = []
    if args.filter_category:
        clauses.append({"op": "Eq", "field": "category", "value": args.filter_category})
    if args.min_year:
        clauses.append({"op": "Gte", "field": "year", "value": args.min_year})
    if not clauses:
        return None
    if len(clauses) == 1:
        return clauses[0]
    return {"op": "And", "clauses": clauses}


def check_provider_compat(client: LagoonClient, namespace: str, provider) -> None:
    ns = client.get_namespace(namespace)
    recorded = (
        ns.get("schema", {}).get("annotations", {}).get("embedding_provider")
    )
    if recorded and recorded != provider.name:
        sys.exit(
            f"error: namespace '{namespace}' was embedded with provider "
            f"'{recorded}' but you are querying with '{provider.name}'. "
            f"Re-run ingest.py with the same provider, or set "
            f"LAGOON_EMBEDDINGS accordingly."
        )


def run_query(client, provider, namespace, question, top_k, filter_expr):
    query_vector = provider.embed([question])[0]
    response = client.query(
        namespace,
        top_k=top_k,
        vector={"values": query_vector, "mode": "auto"},
        filter=filter_expr,
        include_attributes=["title", "body", "category", "year"],
    )
    print(
        f"\n{len(response['results'])} results "
        f"({response.get('took_ms', '?')} ms, "
        f"plan: {response.get('plan', {}).get('vector_strategy', 'n/a')})\n"
    )
    for rank, hit in enumerate(response["results"], start=1):
        attrs = hit.get("attributes", {})
        snippet = textwrap.shorten(attrs.get("body", ""), width=160)
        print(f"{rank}. [{hit['score']:.4f}] {attrs.get('title')} "
              f"({attrs.get('category')}, {attrs.get('year')})")
        print(f"   {snippet}\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("question", nargs="?", help="natural-language query")
    parser.add_argument("--namespace", default=DEFAULT_NAMESPACE)
    parser.add_argument("--provider", default=None)
    parser.add_argument("--top-k", type=int, default=5)
    parser.add_argument("--filter-category", default=None)
    parser.add_argument("--min-year", type=int, default=None)
    parser.add_argument("--interactive", action="store_true")
    args = parser.parse_args()

    if not args.question and not args.interactive:
        parser.error("provide a question or use --interactive")

    provider = get_provider(args.provider)
    client = LagoonClient()
    check_provider_compat(client, args.namespace, provider)
    filter_expr = build_filter(args)

    if args.interactive:
        print("LagoonDB semantic search (empty line to quit)")
        while True:
            try:
                question = input("query> ").strip()
            except (EOFError, KeyboardInterrupt):
                break
            if not question:
                break
            run_query(client, provider, args.namespace, question, args.top_k, filter_expr)
    else:
        run_query(client, provider, args.namespace, args.question, args.top_k, filter_expr)


if __name__ == "__main__":
    main()
