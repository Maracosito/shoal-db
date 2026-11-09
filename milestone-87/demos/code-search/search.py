#!/usr/bin/env python3
"""Hybrid code search over an indexed namespace.

Usage:
    python demos/code-search/search.py "parse csv of readings"
    python demos/code-search/search.py "predict tide levels" --namespace code-feature
    python demos/code-search/search.py "moving average" --kind function --top-k 3
"""
from __future__ import annotations

import argparse
import pathlib
import sys
from typing import List, Optional

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from common.embeddings import get_provider
from common.lagoon_client import LagoonClient


def build_filter(lang: Optional[str], kind: Optional[str], path: Optional[str]) -> Optional[list]:
    clauses: List[list] = []
    if lang:
        clauses.append(["lang", "Eq", lang])
    if kind:
        clauses.append(["kind", "Eq", kind])
    if path:
        clauses.append(["path", "Eq", path])
    if not clauses:
        return None
    return clauses[0] if len(clauses) == 1 else ["And", clauses]


def search(client: LagoonClient, provider, namespace: str, query: str,
           flt: Optional[list], top_k: int) -> List[dict]:
    vector = provider.embed([query])[0]
    return client.query(
        namespace,
        text=query,
        vector=vector,
        mode="hybrid",
        fusion={"method": "rrf", "k": 60},
        fields={"symbol": 2.0, "code": 1.0, "path": 1.5},
        filter=flt,
        top_k=top_k,
        include_attributes=["path", "symbol", "kind", "start_line", "code"],
    )


def print_hits(hits: List[dict], show_code: bool = True) -> None:
    if not hits:
        print("  (no results)")
        return
    for hit in hits:
        a = hit.get("attributes", {})
        loc = f"{a.get('path', '?')}:{a.get('start_line', '?')}"
        print(f"  {hit['score']:6.3f}  {loc}  [{a.get('kind', '?')}] {a.get('symbol', '')}")
        if show_code and a.get("code"):
            snippet = "\n".join(str(a["code"]).splitlines()[:4])
            for line in snippet.splitlines():
                print(f"          | {line}")
    print()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("query")
    ap.add_argument("--namespace", default="code-main")
    ap.add_argument("--provider", default=None)
    ap.add_argument("--top-k", type=int, default=5)
    ap.add_argument("--lang", default=None)
    ap.add_argument("--kind", default=None, choices=[None, "module", "function", "class", "doc"])
    ap.add_argument("--path", default=None, help="exact path filter, e.g. tidegauge/stats.py")
    ap.add_argument("--no-code", action="store_true", help="hide code snippets")
    args = ap.parse_args()

    client = LagoonClient()
    provider = get_provider(args.provider)
    flt = build_filter(args.lang, args.kind, args.path)
    print(f"namespace={args.namespace!r}  query={args.query!r}" + (f"  filter={flt}" if flt else ""))
    hits = search(client, provider, args.namespace, args.query, flt, args.top_k)
    print_hits(hits, show_code=not args.no_code)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
