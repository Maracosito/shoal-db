# Benchmark guide & methodology

This guide explains how to run the Lagoon benchmark suite, what each number
means, and — most importantly — how to report results honestly.

## 1. Principles

1. **Measure what users experience.** Primary numbers are client-observed
   latencies (HTTP + JSON included). Server-internal `took_ms` is recorded
   alongside for diagnosis, never substituted silently.
2. **Disclose the environment.** Every result file embeds timestamp, host,
   platform, CPU count, and the server URL. You must additionally record the
   storage backend (local filesystem / MinIO / real S3 + region), instance
   type, and whether the server was a release build. A number without its
   environment is not a result.
3. **Deterministic datasets.** `datagen.py` produces seeded data: vectors
   from a 64-component Gaussian mixture (L2-normalised) and text from a
   256-word vocabulary with Zipf(1.1) term frequencies. Identical parameters
   ⇒ identical bytes, on any machine.
4. **No unverified comparisons.** See § 7.

## 2. Test matrix

Recommended configurations, smallest first:

| Tier | Docs | Dim | Backend | Purpose |
|---|---|---|---|---|
| smoke | 5,000 | 64 | filesystem | CI sanity, seconds to run |
| dev | 50,000 | 256 | MinIO (Docker) | default; realistic object I/O on one box |
| full | 500,000+ | 768 | real S3, same region | publishable numbers |

Numbers from the filesystem backend say nothing about object-storage latency;
label them as such. MinIO on localhost has near-zero network latency — it
exercises the request *path*, not S3's latency profile. Only same-region S3
results should be quoted as "object-storage performance".

## 3. What each benchmark measures

### 3.1 Ingest (`bench_ingest.py`)

- A batch "counts" when the upsert API returns success, which by Lagoon's
  write contract means the batch is durable in the WAL on the storage
  backend. This is **durable-write throughput**, not buffer throughput.
- **Index catch-up** is reported separately: the time after the last write
  until background indexing reports zero pending WAL entries. Documents are
  queryable through the WAL overlay before catch-up; catch-up affects how
  much of the corpus is served from optimised segments.
- `--concurrency N` runs N closed-loop writer threads. This is a simple
  saturation probe, not an open-loop load test.

### 3.2 Recall (`bench_recall.py`)

- Ground truth is the server's own `mode=exact` brute-force kNN over the same
  namespace — so recall isolates the **ANN index's approximation error**, not
  differences in distance arithmetic.
- `recall@k = |ANN top-k ∩ exact top-k| / k`, averaged over queries (k ∈
  {1, 10, 100}).
- Query vectors are drawn from the same mixture as the corpus (different
  noise), giving realistic difficulty. Uniform-random queries would inflate
  or deflate recall depending on dimension; don't substitute them.
- Always report recall **with** the ANN/exact latency pair. Recall without
  its latency cost (or vice versa) is half a number.

### 3.3 Latency (`bench_latency.py`)

Definitions used throughout:

- **Cold**: the very first query against a namespace after the server process
  starts (or its local cache directory is cleared), forcing manifest and
  segment fetches from object storage. **Only the first query is a true cold
  sample.** Collecting N cold samples requires N restarts/clears; the suite
  refuses to fake this by averaging warm-ish queries into a "cold" figure.
- **Warm**: the namespace has been preloaded via the warm-cache endpoint (or
  prior queries); segments are served from local disk/memory cache.

Warm workloads (default 200 queries each, 5 warmup queries excluded):
`vector_ann`, `vector_exact`, `bm25` (title^2 + body), `hybrid_rrf`,
`filtered_vector` (equality + range filter). Reported: mean, min, p50, p90,
p99, max.

**Coordinated omission caveat:** these are closed-loop, single-in-flight
measurements. They characterise per-request service latency at low load.
They are **not** throughput-under-load numbers and must not be converted to
QPS claims ("p50 is 5 ms therefore 200 QPS" is wrong).

### 3.4 Cache (`bench_cache.py`)

- Scrapes Prometheus `/metrics` before and after a workload and reports the
  **delta** in `lagoon_cache_hits_total` / `lagoon_cache_misses_total` per
  tier, plus object-store GET/PUT counts — so the hit rate is attributable
  to the workload, not the server's lifetime.
- `--hot-set N` cycles through N distinct queries to simulate skewed access;
  with the default (all-distinct queries) hit rates reflect segment reuse
  only, which is the pessimistic case.

## 4. Running the full suite

```bash
cd benchmarks && pip install -r requirements.txt
export LAGOON_URL=http://localhost:8484 LAGOON_API_KEY=dev-key
python run_all.py --docs 50000 --dim 256
# cold samples (repeat 5–10×, restarting the server each time):
docker compose restart api
python bench_latency.py --phase cold --namespace bench-ingest --dim 256
python run_all.py --summary-only
```

Outputs: one JSON file per benchmark under `benchmarks/results/`, plus
`results/SUMMARY.md`. The summary template includes mandatory "FILL IN"
fields (storage backend, server build SHA); a summary with those fields
unfilled is incomplete.

## 5. Run hygiene checklist

Before recording publishable numbers:

- [ ] Release build of the server (not debug).
- [ ] No other heavy processes on the machine; frequency scaling noted if on
      a laptop.
- [ ] Storage backend and network locality recorded (e.g. "S3, us-east-1,
      same-AZ EC2 c6i.2xlarge").
- [ ] Background indexing caught up before latency/recall phases
      (`bench_ingest.py` waits for this automatically).
- [ ] ≥ 5 cold samples collected via real restarts, reported individually or
      as a range — never as a single-sample "average".
- [ ] Each benchmark run at least twice; if run-to-run variance of p50
      exceeds ~10 %, investigate before reporting.
- [ ] Git SHA of the server build recorded in the summary.

## 6. Interpreting results

- **Cold vs warm gap** is dominated by object-store round trips. Expect cold
  latency to scale with the number of segments/manifest objects fetched;
  warming or pinning a namespace removes this from the query path. A large
  gap is the *design working as intended* (cheap cold storage, fast warm
  serving), not a defect — but report both numbers.
- **Recall tuning**: IVF recall rises with the number of probed centroid
  lists, at proportional latency cost. If you change `nprobe`-style settings
  from the server defaults, say so in the results.
- **BM25 latency** depends on term frequency: queries made of common (Zipf
  head) terms touch long posting lists. The generated query terms follow the
  same Zipf distribution as the corpus, which is the realistic case.
- **Index catch-up** is a freshness metric, not a correctness one: documents
  are queryable from the WAL overlay immediately after a successful upsert.

## 7. Honest reporting policy

- Report only numbers actually produced by these scripts (or clearly marked
  derivatives), with their environment blocks.
- **No comparisons with other products** — including the commercial systems
  that inspired Lagoon — unless you measured both systems yourself, on the
  same machine, same dataset, same query set, same day, and you publish both
  raw result sets. This repository makes **no performance-parity claims**
  with any proprietary system, and contributions adding such claims without
  the above evidence will be declined.
- Never average a single cold sample into a distribution; never relabel
  closed-loop latency as throughput; never quote MinIO-on-localhost numbers
  as "S3 performance".
- This repository deliberately ships **no committed benchmark numbers**:
  hardware varies too much for repository-blessed figures to be honest. Run
  the suite on your target environment; `results/` is gitignored.

## 8. CI smoke benchmark

For regression detection (not absolute performance), CI can run the smoke
tier against the filesystem backend:

```bash
python run_all.py --docs 5000 --dim 64 --queries 50 --recall-queries 25
```

Treat CI numbers as *relative trend* signals only — shared CI runners have
noisy neighbours. A sensible CI gate is on **recall@10** (a correctness-like
property, stable across hardware), not on latency.
