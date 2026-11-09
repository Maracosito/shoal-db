"""Cache hit-rate benchmark.

Scrapes the server's Prometheus /metrics endpoint before and after a query
workload and reports the delta in cache hits/misses per tier, giving the hit
rate attributable to the workload itself (not the server's whole lifetime).

Default metric names (override with flags if your build differs):
    lagoon_cache_hits_total{tier="memory"|"disk"}
    lagoon_cache_misses_total{tier="memory"|"disk"}
    lagoon_object_store_get_total
    lagoon_object_store_put_total

Workload: a configurable number of vector + BM25 queries over a namespace,
optionally repeated over a small "hot set" of queries to simulate skew.

Usage:
    python bench_cache.py --namespace bench-ingest --dim 256 --queries 200 \
        --hot-set 20
"""

from __future__ import annotations

import argparse
from typing import Dict

from common import (
    BenchReport,
    LagoonBenchClient,
    parse_prometheus_text,
    text_query,
    vector_query,
)
from datagen import generate_query_terms, generate_query_vectors


def labelled_sums(samples: Dict[str, float], metric: str) -> Dict[str, float]:
    """Return {label_string_or_'': value_sum} for one metric name."""
    out: Dict[str, float] = {}
    for key, value in samples.items():
        bare, _, labels = key.partition("{")
        if bare != metric:
            continue
        label_key = "{" + labels if labels else ""
        out[label_key] = out.get(label_key, 0.0) + value
    return out


def delta(before: Dict[str, float], after: Dict[str, float]) -> Dict[str, float]:
    keys = set(before) | set(after)
    return {k: after.get(k, 0.0) - before.get(k, 0.0) for k in sorted(keys)}


def main() -> None:
    ap = argparse.ArgumentParser(description="Lagoon cache hit-rate benchmark")
    ap.add_argument("--namespace", default="bench-ingest")
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--queries", type=int, default=200)
    ap.add_argument("--hot-set", type=int, default=0,
                    help="If > 0, cycle through this many distinct queries "
                         "instead of all-unique queries (simulates skew).")
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--hits-metric", default="lagoon_cache_hits_total")
    ap.add_argument("--misses-metric", default="lagoon_cache_misses_total")
    ap.add_argument("--store-get-metric", default="lagoon_object_store_get_total")
    ap.add_argument("--store-put-metric", default="lagoon_object_store_put_total")
    ap.add_argument("--out-dir", default="results")
    args = ap.parse_args()

    client = LagoonBenchClient()

    n_distinct = args.hot_set if args.hot_set > 0 else args.queries
    qvecs = generate_query_vectors(n_distinct, args.dim, args.seed)
    qterms = generate_query_terms(n_distinct, args.seed)

    before = parse_prometheus_text(client.metrics_text())

    print(f"Running {args.queries} queries "
          f"({n_distinct} distinct, hot-set={'on' if args.hot_set else 'off'})...")
    for i in range(args.queries):
        j = i % n_distinct
        if i % 2 == 0:
            client.query(args.namespace, vector_query(qvecs[j], top_k=10, mode="ann"))
        else:
            client.query(args.namespace, text_query(qterms[j], top_k=10))

    after = parse_prometheus_text(client.metrics_text())

    hits_delta = delta(labelled_sums(before, args.hits_metric),
                       labelled_sums(after, args.hits_metric))
    misses_delta = delta(labelled_sums(before, args.misses_metric),
                         labelled_sums(after, args.misses_metric))
    store_gets = sum(delta(labelled_sums(before, args.store_get_metric),
                           labelled_sums(after, args.store_get_metric)).values())
    store_puts = sum(delta(labelled_sums(before, args.store_put_metric),
                           labelled_sums(after, args.store_put_metric)).values())

    total_hits = sum(hits_delta.values())
    total_misses = sum(misses_delta.values())
    denom = total_hits + total_misses
    hit_rate = round(total_hits / denom, 4) if denom > 0 else None

    results = {
        "queries_issued": args.queries,
        "distinct_queries": n_distinct,
        "cache_hits_by_tier": hits_delta,
        "cache_misses_by_tier": misses_delta,
        "overall_hit_rate": hit_rate,
        "object_store_gets": store_gets,
        "object_store_puts": store_puts,
    }
    if denom == 0:
        results["warning"] = (
            "No cache hit/miss counter movement observed. Check the metric "
            "names against your server build's /metrics output and pass "
            "--hits-metric/--misses-metric accordingly."
        )

    report = BenchReport(
        name="cache",
        params={
            "namespace": args.namespace,
            "dim": args.dim,
            "queries": args.queries,
            "hot_set": args.hot_set,
            "seed": args.seed,
        },
        results=results,
    )
    report.print_summary()
    path = report.save(args.out_dir)
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
