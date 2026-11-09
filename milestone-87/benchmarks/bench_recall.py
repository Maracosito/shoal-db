"""ANN recall benchmark: recall@k of approximate search vs exact kNN.

Method:
  1. For each query vector, run the SAME query twice against the SAME
     namespace: once with mode="exact" (brute-force ground truth) and once
     with mode="ann" (IVF index).
  2. recall@k = |ann_top_k ∩ exact_top_k| / k, averaged over all queries.
  3. Report recall at k in {1, 10, 100} (clipped to --top-k), plus the
     latency of each mode so the recall/latency tradeoff is visible.

Ties: with float scores exact ties are rare in this synthetic dataset; we
treat the server's returned ordering as authoritative and do not attempt tie
normalisation.  If you change the dataset, note the tie behaviour in your
results.

Usage (against a namespace loaded by bench_ingest.py --keep-namespace):
    python bench_recall.py --namespace bench-ingest --dim 256 --queries 100 \
        --top-k 100
"""

from __future__ import annotations

import argparse
from typing import List, Set

from common import BenchReport, LagoonBenchClient, summarize_latencies, timed_call, vector_query
from datagen import generate_query_vectors


def result_ids(response: dict) -> List[str]:
    return [r["id"] for r in response.get("results", [])]


def main() -> None:
    ap = argparse.ArgumentParser(description="Lagoon ANN recall@k benchmark")
    ap.add_argument("--namespace", default="bench-ingest")
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--queries", type=int, default=100)
    ap.add_argument("--top-k", type=int, default=100)
    ap.add_argument("--metric", default="cosine",
                    choices=["cosine", "dot", "euclidean"])
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out-dir", default="results")
    args = ap.parse_args()

    ks = sorted({k for k in (1, 10, 100) if k <= args.top_k})
    client = LagoonBenchClient()
    qvecs = generate_query_vectors(args.queries, args.dim, args.seed)

    # Warm caches first so we measure index quality, not cold I/O.
    try:
        client.warm_namespace(args.namespace)
    except RuntimeError:
        pass
    client.query(args.namespace,
                 vector_query(qvecs[0], top_k=args.top_k, mode="exact",
                              metric=args.metric))

    exact_ms: List[float] = []
    ann_ms: List[float] = []
    recall_sums = {k: 0.0 for k in ks}

    for i in range(args.queries):
        q_exact = vector_query(qvecs[i], top_k=args.top_k, mode="exact",
                               metric=args.metric)
        q_ann = vector_query(qvecs[i], top_k=args.top_k, mode="ann",
                             metric=args.metric)

        exact_resp, e_ms = timed_call(client.query, args.namespace, q_exact)
        ann_resp, a_ms = timed_call(client.query, args.namespace, q_ann)
        exact_ms.append(e_ms)
        ann_ms.append(a_ms)

        exact_ids = result_ids(exact_resp)
        ann_ids = result_ids(ann_resp)
        for k in ks:
            truth: Set[str] = set(exact_ids[:k])
            if not truth:
                continue
            hit = len(truth.intersection(ann_ids[:k]))
            recall_sums[k] += hit / float(len(truth))

        if (i + 1) % 25 == 0:
            print(f"  {i + 1}/{args.queries} queries done")

    recalls = {f"recall@{k}": round(recall_sums[k] / args.queries, 4) for k in ks}

    report = BenchReport(
        name="recall",
        params={
            "namespace": args.namespace,
            "dim": args.dim,
            "queries": args.queries,
            "top_k": args.top_k,
            "metric": args.metric,
            "seed": args.seed,
        },
        results={
            **recalls,
            "exact_latency": summarize_latencies(exact_ms),
            "ann_latency": summarize_latencies(ann_ms),
            "speedup_mean": round(
                (sum(exact_ms) / len(exact_ms)) / (sum(ann_ms) / len(ann_ms)), 2
            ) if ann_ms and sum(ann_ms) > 0 else None,
        },
    )
    report.print_summary()
    path = report.save(args.out_dir)
    print(f"Saved {path}")


if __name__ == "__main__":
    main()
