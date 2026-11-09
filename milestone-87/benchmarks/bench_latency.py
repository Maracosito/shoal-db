"""Query latency benchmark: vector / BM25 / hybrid, cold vs warm.

Definitions used here (be precise when reporting results):

  * COLD query: the API server process has just started (or its local disk
    cache directory was cleared) and no requests have touched the target
    namespace, so the first query must fetch manifest + segment data from
    object storage.  Only the FIRST query against the namespace is a true
    cold sample; we record it separately rather than averaging it away.
    To collect N cold samples you must restart/clear N times — this script
    automates one sample per invocation in --phase cold and run_all.py
    explains how to repeat it.

  * WARM query: the namespace has been explicitly warmed via the warm-cache
    endpoint (or previously queried), so segments are served from the local
    disk/memory cache.

Workloads:
  * vector  — top-k dense ANN search (and exact mode for comparison)
  * bm25    — full-text BM25 over title^2 + body
  * hybrid  — RRF fusion of the two
  * filtered-vector — vector search with an attribute filter

Usage (against a namespace previously loaded by bench_ingest.py
--keep-namespace):

    # one cold sample (run right after a server restart / cache clear):
    python bench_latency.py --namespace bench-ingest --dim 256 --phase cold

    # warm measurement:
    python bench_latency.py --namespace bench-ingest --dim 256 --phase warm \
        --queries 200
"""

from __future__ import annotations

import argparse
from typing import List

from common import (
    BenchReport,
    LagoonBenchClient,
    hybrid_query,
    run_query_workload,
    summarize_latencies,
    text_query,
    timed_call,
    vector_query,
)
from datagen import CATEGORIES, generate_query_terms, generate_query_vectors


def build_workloads(dim: int, n_queries: int, top_k: int, seed: int) -> dict:
    qvecs = generate_query_vectors(n_queries, dim, seed)
    qterms = generate_query_terms(n_queries, seed)
    return {
        "vector_ann": [
            vector_query(qvecs[i], top_k=top_k, mode="ann") for i in range(n_queries)
        ],
        "vector_exact": [
            vector_query(qvecs[i], top_k=top_k, mode="exact") for i in range(n_queries)
        ],
        "bm25": [
            text_query(qterms[i], fields=["title^2", "body"], top_k=top_k)
            for i in range(n_queries)
        ],
        "hybrid_rrf": [
            hybrid_query(qvecs[i], qterms[i], top_k=top_k, fusion="rrf")
            for i in range(n_queries)
        ],
        "filtered_vector": [
            vector_query(
                qvecs[i],
                top_k=top_k,
                mode="ann",
                filter_expr={
                    "op": "and",
                    "filters": [
                        {"op": "eq", "attribute": "category",
                         "value": CATEGORIES[i % len(CATEGORIES)]},
                        {"op": "gte", "attribute": "year", "value": 2015},
                    ],
                },
            )
            for i in range(n_queries)
        ],
    }


def run_cold(client: LagoonBenchClient, ns: str, dim: int, top_k: int, seed: int) -> dict:
    """Take exactly one cold sample per workload type would re-warm the cache
    after the first request — so a single invocation records ONE cold sample
    (the very first query) and then the immediately-following 'first-touch'
    latencies for the other workloads, which are 'partially warm' (manifest
    cached, some segments cached).  Both are reported, clearly labelled."""
    qvec = generate_query_vectors(1, dim, seed)[0]
    qterm = generate_query_terms(1, seed)[0]

    results = {}
    _, cold_ms = timed_call(client.query, ns, vector_query(qvec, top_k=top_k, mode="ann"))
    results["first_query_cold_ms"] = round(cold_ms, 3)

    _, bm25_ms = timed_call(client.query, ns, text_query(qterm, top_k=top_k))
    results["bm25_after_cold_ms"] = round(bm25_ms, 3)

    _, hyb_ms = timed_call(client.query, ns, hybrid_query(qvec, qterm, top_k=top_k))
    results["hybrid_after_cold_ms"] = round(hyb_ms, 3)

    results["note"] = (
        "Only first_query_cold_ms is a true cold sample. Repeat this phase "
        "after a server restart / cache clear to collect more samples."
    )
    return results


def run_warm(
    client: LagoonBenchClient, ns: str, dim: int, n_queries: int, top_k: int, seed: int
) -> dict:
    print("Warming namespace cache via warm endpoint...")
    try:
        client.warm_namespace(ns)
    except RuntimeError as exc:
        print(f"  warm endpoint failed ({exc}); falling back to query-driven warmup")

    workloads = build_workloads(dim, n_queries, top_k, seed)
    out = {}
    for name, queries in workloads.items():
        print(f"Running warm workload '{name}' ({len(queries)} queries)...")
        out[name] = run_query_workload(client, ns, queries, warmup=5)
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description="Lagoon query latency benchmark")
    ap.add_argument("--namespace", default="bench-ingest")
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--queries", type=int, default=200)
    ap.add_argument("--top-k", type=int, default=10)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--phase", choices=["cold", "warm"], default="warm")
    ap.add_argument("--out-dir", default="results")
    args = ap.parse_args()

    client = LagoonBenchClient()

    if args.phase == "cold":
        results = run_cold(client, args.namespace, args.dim, args.top_k, args.seed)
        name = "latency_cold"
    else:
        results = run_warm(
            client, args.namespace, args.dim, args.queries, args.top_k, args.seed
        )
        name = "latency_warm"

    report = BenchReport(
        name=name,
        params={
            "namespace": args.namespace,
            "dim": args.dim,
            "queries": args.queries if args.phase == "warm" else 1,
            "top_k": args.top_k,
            "seed": args.seed,
            "phase": args.phase,
        },
        results=results,
    )
    report.print_summary()
    path = report.save(args.out_dir)
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
