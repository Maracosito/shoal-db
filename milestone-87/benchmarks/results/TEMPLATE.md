# Lagoon Benchmark Results — REPORT TEMPLATE

> Copy this file to `benchmarks/results/<YYYY-MM-DD>-<short-env-name>.md`,
> run the suite, and fill in every section. A results report that omits any
> required section below should not be merged or published.
>
> **Reporting policy (non-negotiable):**
> 1. Every number must come from a run of the bundled suite
>    (`python benchmarks/run_all.py`) and be reproducible from the recorded
>    configuration in this report.
> 2. No comparisons to other systems (Turbopuffer, Elasticsearch, Qdrant,
>    pgvector, …) unless *that system was benchmarked by you, on the same
>    hardware, with its configuration disclosed in the same report*. Quoting
>    other vendors' published numbers next to ours is not a comparison; it is
>    marketing, and it does not belong here.
> 3. Report medians and tail percentiles, never best-of-N. Report failures
>    and anomalies; do not silently re-run until the numbers look good.
> 4. Disclose anything that could flatter the results: page cache state,
>    co-located MinIO, dataset fitting in RAM, etc.

---

## 1. Summary

_Two or three sentences. What was measured, on what scale of data, and the
single most important caveat. No superlatives._

## 2. Environment

| Item | Value |
|---|---|
| Date of run | |
| Lagoon version / git commit | `git rev-parse HEAD` |
| Host | (e.g. "Hetzner CCX33", "M2 MacBook Pro", "c6id.2xlarge") |
| CPU (model, cores) | `lscpu` / `sysctl -n machdep.cpu.brand_string` |
| RAM | |
| Local disk used for cache | (device type — NVMe / SATA SSD / network volume — matters a lot) |
| OS / kernel | |
| Object storage backend | (MinIO local / MinIO remote / AWS S3 region / `file://`) |
| Network between server and object store | (loopback / LAN / WAN; approximate RTT if remote) |
| Server config deltas from defaults | (cache sizes, indexing interval, IVF nlist/nprobe, …) |
| Concurrent load on the machine during the run | (should be "none"; disclose otherwise) |

**Topology disclosure:** state explicitly whether the benchmark client, the
Lagoon server, and the object store ran on the same machine. Loopback MinIO
numbers measure the engine, not a realistic deployment; that is fine, but say
so.

## 3. Dataset

| Item | Value |
|---|---|
| Generator invocation | (exact `benchmarks/datagen.py` command line, including seed) |
| Document count | |
| Vector dimensions | |
| Vector distribution | (clustered-Gaussian from datagen / real embeddings — name the source) |
| Text field characteristics | (vocabulary size, mean tokens/doc, from datagen output) |
| Filterable attributes | |
| Total logical data size | |

If real embeddings were used instead of the synthetic generator, name the
model and the source corpus, and note that recall numbers are not directly
comparable to synthetic-data runs.

## 4. Ingest throughput (`bench_ingest.py`)

Paste the JSON summary emitted by the tool, then the table:

| Batch size | Docs/sec (median) | MB/sec | WAL commits | p99 batch latency (ms) |
|---|---|---|---|---|
| | | | | |

Notes to fill in:
- Was background indexing enabled during ingest? (It is by default; disabling
  it inflates throughput and must be disclosed.)
- Indexing lag at end of ingest (from `/metrics`,
  `lagoon_indexing_lag_documents`): how many documents were durable but not
  yet ANN/FTS-indexed when ingest finished, and how long until lag reached 0.

## 5. Query latency — cold vs warm (`bench_latency.py`)

One table per query class. Cold = caches dropped via the bench harness
(server restart + disk cache purge) before each measured query; warm =
steady state after the warm-up phase. Report ms.

### 5.1 Vector ANN (IVF, top_k=10, nprobe as configured)

| | p50 | p90 | p99 | mean | n |
|---|---|---|---|---|---|
| Cold | | | | | |
| Warm | | | | | |

### 5.2 Vector exact kNN (baseline)

| | p50 | p90 | p99 | mean | n |
|---|---|---|---|---|---|
| Cold | | | | | |
| Warm | | | | | |

### 5.3 Full-text BM25 (top_k=10)

| | p50 | p90 | p99 | mean | n |
|---|---|---|---|---|---|
| Cold | | | | | |
| Warm | | | | | |

### 5.4 Hybrid (vector + BM25, RRF and weighted fusion)

| Fusion | State | p50 | p90 | p99 | n |
|---|---|---|---|---|---|
| RRF | Cold | | | | |
| RRF | Warm | | | | |
| Weighted | Warm | | | | |

### 5.5 Filtered vector query

| Filter selectivity | State | p50 | p99 | n |
|---|---|---|---|---|
| ~10% | Warm | | | |
| ~1% | Warm | | | |

**Required commentary:** explain the cold/warm gap in terms of object-storage
round trips (the harness prints `lagoon_object_store_get_total` deltas per
query class — include them). If cold numbers benefit from a warm OS page cache
on a co-located MinIO, say so.

## 6. ANN recall vs exact (`bench_recall.py`)

| nprobe | recall@1 | recall@10 | recall@100 | warm p50 (ms) |
|---|---|---|---|---|
| | | | | |
| | | | | |
| | | | | |

Plot or describe the recall/latency trade-off curve. State the IVF `nlist`
used and the number of query vectors evaluated (minimum 1,000 for a
publishable report). Ground truth must come from the suite's exact-kNN pass
over the same data, never from a precomputed file of unknown provenance.

## 7. Cache behavior (`bench_cache.py`)

| Metric | Value |
|---|---|
| Disk cache hit rate, steady state | |
| Memory cache hit rate, steady state | |
| Queries to reach >90% disk hit rate from cold | |
| Effect of `warm` endpoint: first-query latency, pre-warmed vs cold | |
| Eviction behavior under cache pressure (configured cache < working set) | |

## 8. Anomalies, failures, and re-runs

_Mandatory section, even if empty of incidents. List every benchmark
invocation that errored, every outlier you investigated, and every re-run with
its reason. "Re-ran because the numbers looked wrong" is a legitimate entry —
hiding it is not._

## 9. Reproduction

```bash
# exact commands, in order, including environment variables and seeds
```

The raw JSON output of each tool (written to `benchmarks/results/raw/` by
`run_all.py`) should be committed alongside this report or attached to the PR.

## 10. Interpretation guardrails

Before publishing, confirm each statement and check the box in the PR:

- [ ] No comparison to any system not benchmarked in this report.
- [ ] All hardware/topology/config disclosed in §2.
- [ ] Medians + tails reported; no best-of-N anywhere.
- [ ] Cold-query methodology (cache drop procedure) described and consistent
      with `bench_latency.py`'s harness.
- [ ] Recall ground truth computed by the suite's exact pass.
- [ ] Anomalies section completed.
- [ ] Dataset seed recorded so the run is reproducible bit-for-bit.
