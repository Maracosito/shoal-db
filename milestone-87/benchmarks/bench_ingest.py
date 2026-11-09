"""Ingest throughput benchmark.

Measures end-to-end upsert throughput (documents/second and MB/second of JSON
payload) against a Lagoon server, and separately measures how long background
indexing takes to catch up after the last write (indexing lag).

What is and is not measured:
  * Measured: client-observed durable-write throughput.  A batch counts as
    ingested when the upsert HTTP call returns success, which per Lagoon's
    write-path contract means the batch is durable in the WAL on object
    storage.
  * Measured separately: time until background indexing reports zero pending
    WAL entries ("index catch-up").  Documents are queryable via the WAL
    overlay before that point; catch-up affects ANN/BM25 segment freshness.
  * Not measured: concurrent multi-writer throughput (use --concurrency to
    enable a simple thread-pool mode; results are still closed-loop per
    thread).

Usage:
    python bench_ingest.py --docs 50000 --dim 256 --batch-size 500
    python bench_ingest.py --docs 50000 --dim 256 --batch-size 500 --concurrency 4
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import time
from typing import List

from common import BenchReport, LagoonBenchClient, summarize_latencies
from datagen import generate_documents


def chunked(items: List[dict], size: int) -> List[List[dict]]:
    return [items[i : i + size] for i in range(0, len(items), size)]


def main() -> None:
    ap = argparse.ArgumentParser(description="Lagoon ingest throughput benchmark")
    ap.add_argument("--docs", type=int, default=50_000)
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--batch-size", type=int, default=500)
    ap.add_argument("--concurrency", type=int, default=1)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--namespace", default="bench-ingest")
    ap.add_argument("--keep-namespace", action="store_true",
                    help="Do not delete the namespace afterwards (reuse it for "
                         "the latency/recall benchmarks).")
    ap.add_argument("--out-dir", default="results")
    args = ap.parse_args()

    client = LagoonBenchClient()

    print(f"Generating {args.docs} documents (dim={args.dim}, seed={args.seed})...")
    docs = list(generate_documents(args.docs, args.dim, args.seed))
    batches = chunked(docs, args.batch_size)
    payload_bytes = sum(len(json.dumps({"documents": b}).encode()) for b in batches)

    print(f"Recreating namespace '{args.namespace}'...")
    client.delete_namespace(args.namespace)
    client.create_namespace(args.namespace, vector_dim=args.dim)

    print(f"Ingesting {len(batches)} batches of {args.batch_size} "
          f"(concurrency={args.concurrency})...")
    batch_latencies_ms: List[float] = []
    t_start = time.perf_counter()

    if args.concurrency <= 1:
        for i, batch in enumerate(batches):
            t0 = time.perf_counter()
            client.upsert(args.namespace, batch)
            batch_latencies_ms.append((time.perf_counter() - t0) * 1000.0)
            if (i + 1) % 20 == 0:
                done = (i + 1) * args.batch_size
                rate = done / (time.perf_counter() - t_start)
                print(f"  {done}/{args.docs} docs ({rate:,.0f} docs/s)")
    else:
        def send(batch: List[dict]) -> float:
            local = LagoonBenchClient()  # one session per worker
            t0 = time.perf_counter()
            local.upsert(args.namespace, batch)
            return (time.perf_counter() - t0) * 1000.0

        with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
            for elapsed in pool.map(send, batches):
                batch_latencies_ms.append(elapsed)

    ingest_wall_s = time.perf_counter() - t_start
    docs_per_s = args.docs / ingest_wall_s
    mb_per_s = (payload_bytes / 1e6) / ingest_wall_s

    print("Waiting for background indexing to catch up...")
    t_idx0 = time.perf_counter()
    caught_up = client.wait_for_indexing(args.namespace, timeout_s=600)
    index_catchup_s = time.perf_counter() - t_idx0

    report = BenchReport(
        name="ingest",
        params={
            "docs": args.docs,
            "dim": args.dim,
            "batch_size": args.batch_size,
            "concurrency": args.concurrency,
            "seed": args.seed,
            "payload_mb": round(payload_bytes / 1e6, 2),
        },
        results={
            "ingest_wall_seconds": round(ingest_wall_s, 2),
            "docs_per_second": round(docs_per_s, 1),
            "payload_mb_per_second": round(mb_per_s, 2),
            "batch_latency": summarize_latencies(batch_latencies_ms),
            "index_catchup_seconds": round(index_catchup_s, 2),
            "index_caught_up": caught_up,
        },
    )
    report.print_summary()
    path = report.save(args.out_dir)
    print(f"Saved {path}")

    if not args.keep_namespace:
        client.delete_namespace(args.namespace)
        print(f"Deleted namespace '{args.namespace}'")
    else:
        print(f"Kept namespace '{args.namespace}' for follow-up benchmarks.")


if __name__ == "__main__":
    main()
