# Lagoon benchmark suite

Reproducible benchmarks for Lagoon covering:

| Benchmark | Script | Measures |
|---|---|---|
| Ingest | `bench_ingest.py` | upsert throughput (docs/s, MB/s), per-batch latency, index catch-up time |
| Recall | `bench_recall.py` | recall@1/10/100 of ANN (IVF) vs exact kNN, plus latency of both modes |
| Latency | `bench_latency.py` | cold vs warm latency for vector, BM25, hybrid (RRF), filtered vector |
| Cache | `bench_cache.py` | memory/disk cache hit rates and object-store request counts per workload |
| All | `run_all.py` | orchestrates the above and renders `results/SUMMARY.md` |

Read **[docs/benchmark-guide.md](../docs/benchmark-guide.md)** for methodology,
definitions (what "cold" means, how recall is computed), and the honest
reporting policy before publishing any numbers.

## Quickstart

```bash
# 1. Start Lagoon (filesystem backend for a smoke run, MinIO for realistic I/O)
docker compose up -d            # from the repo root

# 2. Install benchmark deps
cd benchmarks
pip install -r requirements.txt

# 3. Run everything (≈50k docs, 256-d vectors)
export LAGOON_URL=http://localhost:8484
export LAGOON_API_KEY=dev-key
python run_all.py --docs 50000 --dim 256

# 4. Collect cold samples (requires a server restart per sample)
docker compose restart api
python bench_latency.py --phase cold --namespace bench-ingest --dim 256
python run_all.py --summary-only

# 5. Read results
cat results/SUMMARY.md
```

All datasets are generated deterministically by `datagen.py` (seeded Gaussian
mixture vectors + Zipf-distributed text), so runs are comparable across
machines and versions. Results are written as JSON per benchmark plus a
rendered Markdown summary; nothing is published automatically.
