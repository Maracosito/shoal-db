use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use futures::stream::{self, StreamExt};
use hdrhistogram::Histogram;
use indicatif::{ProgressBar, ProgressStyle};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

use crate::api::{ApiClient, Document, Fusion, QueryRequest};
use crate::output;

/// Vocabulary used for synthetic text generation so BM25 benchmarks have
/// realistic-ish term distributions without shipping a dataset.
const WORDS: &[&str] = &[
    "harbor", "lantern", "granite", "meadow", "ledger", "compass", "willow", "ember",
    "quartz", "saffron", "thicket", "voyage", "anchor", "breeze", "cinder", "drift",
    "estuary", "fjord", "glacier", "horizon", "isthmus", "juniper", "kelp", "lagoon",
    "marsh", "nebula", "orchard", "plateau", "quarry", "ripple", "summit", "tundra",
    "upland", "valley", "wharf", "xenon", "yarrow", "zephyr", "basalt", "current",
    "delta", "eddy", "flume", "grove", "inlet", "jetty", "knoll", "limestone",
    "moraine", "narrows", "outcrop", "pier", "reef", "shoal", "trench", "undertow",
    "vortex", "wake", "channel", "bluff", "cove", "dune", "escarpment", "floodplain",
];

#[derive(Subcommand)]
pub enum BenchCommand {
    /// Measure upsert throughput with synthetic documents
    Ingest(IngestArgs),
    /// Measure query latency (vector, text, or hybrid)
    Query(QueryBenchArgs),
}

#[derive(Args)]
pub struct IngestArgs {
    /// Target namespace (created with --create if missing)
    #[arg(short = 'n', long)]
    pub namespace: String,
    /// Total number of documents to write
    #[arg(long, default_value_t = 10_000)]
    pub docs: usize,
    /// Vector dimensionality
    #[arg(long, default_value_t = 128)]
    pub dims: usize,
    /// Documents per upsert batch
    #[arg(long, default_value_t = 500)]
    pub batch_size: usize,
    /// Concurrent in-flight batches
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
    /// Also generate a synthetic `text` attribute (enables BM25 query benchmarks)
    #[arg(long)]
    pub with_text: bool,
    /// RNG seed for reproducible datasets
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// Create the namespace if it does not exist
    #[arg(long)]
    pub create: bool,
}

#[derive(Args)]
pub struct QueryBenchArgs {
    /// Namespace to query
    #[arg(short = 'n', long)]
    pub namespace: String,
    /// Number of measured queries
    #[arg(long, default_value_t = 200)]
    pub queries: usize,
    /// Concurrent in-flight queries
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,
    /// top_k per query
    #[arg(short = 'k', long, default_value_t = 10)]
    pub top_k: usize,
    /// Query mode
    #[arg(long, value_enum, default_value_t = BenchQueryMode::Vector)]
    pub mode: BenchQueryMode,
    /// Vector dimensionality (must match the namespace; used to generate query vectors)
    #[arg(long, default_value_t = 128)]
    pub dims: usize,
    /// Warm the namespace cache before measuring (compares against the cold first request)
    #[arg(long)]
    pub warm: bool,
    /// RNG seed for reproducible query workloads
    #[arg(long, default_value_t = 7)]
    pub seed: u64,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum BenchQueryMode {
    Vector,
    Text,
    Hybrid,
}

pub async fn run(client: &ApiClient, cmd: BenchCommand, json_mode: bool) -> Result<()> {
    match cmd {
        BenchCommand::Ingest(args) => run_ingest(client, args, json_mode).await,
        BenchCommand::Query(args) => run_query(client, args, json_mode).await,
    }
}

// ---------------------------------------------------------------------------
// Ingest benchmark
// ---------------------------------------------------------------------------

async fn run_ingest(client: &ApiClient, args: IngestArgs, json_mode: bool) -> Result<()> {
    if args.create {
        match client
            .create_namespace(&args.namespace, Some(args.dims), Some("cosine"))
            .await
        {
            Ok(_) => {}
            Err(e) if e.to_string().contains("HTTP 409") => {}
            Err(e) => return Err(e),
        }
    }

    let batch_size = args.batch_size.max(1);
    let num_batches = args.docs.div_ceil(batch_size);

    let progress = if json_mode {
        None
    } else {
        let pb = ProgressBar::new(args.docs as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "{bar:40} {pos}/{len} docs ({per_sec}, eta {eta})",
            )
            .expect("valid progress template"),
        );
        Some(pb)
    };

    let start = Instant::now();
    let results: Vec<Result<(usize, u64)>> = stream::iter((0..num_batches).map(|batch_idx| {
        let client = client.clone();
        let namespace = args.namespace.clone();
        let progress = progress.clone();
        let docs = args.docs;
        let dims = args.dims;
        let with_text = args.with_text;
        let seed = args.seed;
        async move {
            let first = batch_idx * batch_size;
            let last = ((batch_idx + 1) * batch_size).min(docs);
            // Per-batch deterministic RNG keeps generation reproducible
            // regardless of completion order.
            let mut rng = StdRng::seed_from_u64(seed.wrapping_add(batch_idx as u64));
            let batch: Vec<Document> = (first..last)
                .map(|i| gen_doc(&mut rng, i, dims, with_text))
                .collect();
            let approx_bytes: u64 = batch
                .iter()
                .map(|d| estimate_doc_bytes(d))
                .sum();
            let count = batch.len();
            let key = format!("lakefin-bench-{seed}-{batch_idx}");
            client
                .upsert(&namespace, &batch, Some(&key))
                .await
                .with_context(|| format!("upserting bench batch {batch_idx}"))?;
            if let Some(pb) = &progress {
                pb.inc(count as u64);
            }
            Ok((count, approx_bytes))
        }
    }))
    .buffer_unordered(args.concurrency.max(1))
    .collect()
    .await;
    let wall = start.elapsed();

    if let Some(pb) = &progress {
        pb.finish_and_clear();
    }

    let mut written = 0usize;
    let mut bytes = 0u64;
    let mut errors = 0usize;
    let mut first_error: Option<String> = None;
    for r in results {
        match r {
            Ok((n, b)) => {
                written += n;
                bytes += b;
            }
            Err(e) => {
                errors += 1;
                first_error.get_or_insert_with(|| format!("{e:#}"));
            }
        }
    }

    let secs = wall.as_secs_f64().max(1e-9);
    let docs_per_sec = written as f64 / secs;
    let mb_per_sec = (bytes as f64 / (1024.0 * 1024.0)) / secs;

    let report = json!({
        "benchmark": "ingest",
        "namespace": args.namespace,
        "documents_written": written,
        "failed_batches": errors,
        "first_error": first_error,
        "dims": args.dims,
        "batch_size": batch_size,
        "concurrency": args.concurrency,
        "wall_seconds": round2(secs),
        "docs_per_second": round2(docs_per_sec),
        "approx_mb_per_second": round2(mb_per_sec),
    });

    if json_mode {
        output::print_json(&report);
    } else {
        println!("ingest benchmark");
        println!("  documents written : {written}");
        if errors > 0 {
            println!("  failed batches    : {errors} (first error: {})", first_error.as_deref().unwrap_or("?"));
        }
        println!("  wall time         : {:.2} s", secs);
        println!("  throughput        : {:.0} docs/s ({:.2} MB/s payload)", docs_per_sec, mb_per_sec);
    }
    Ok(())
}

fn gen_doc(rng: &mut StdRng, idx: usize, dims: usize, with_text: bool) -> Document {
    let vector: Vec<f32> = (0..dims).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "category".to_string(),
        json!(WORDS[rng.gen_range(0..WORDS.len())]),
    );
    attributes.insert("rank".to_string(), json!(rng.gen_range(0..100)));
    if with_text {
        let n_words = rng.gen_range(8..24);
        let words: Vec<&str> = (0..n_words)
            .map(|_| WORDS[rng.gen_range(0..WORDS.len())])
            .collect();
        attributes.insert("text".to_string(), json!(words.join(" ")));
    }
    Document {
        id: format!("bench-{idx}"),
        vector: Some(vector),
        sparse_vector: None,
        attributes,
    }
}

fn estimate_doc_bytes(d: &Document) -> u64 {
    let vec_bytes = d.vector.as_ref().map(|v| v.len() * 4).unwrap_or(0);
    let attr_bytes: usize = d
        .attributes
        .values()
        .map(|v| v.to_string().len())
        .sum();
    (d.id.len() + vec_bytes + attr_bytes) as u64
}

// ---------------------------------------------------------------------------
// Query benchmark
// ---------------------------------------------------------------------------

async fn run_query(client: &ApiClient, args: QueryBenchArgs, json_mode: bool) -> Result<()> {
    let mut rng = StdRng::seed_from_u64(args.seed);
    let requests: Vec<QueryRequest> = (0..args.queries.max(1))
        .map(|_| gen_query(&mut rng, &args))
        .collect();

    // Cold measurement: the very first query before any explicit warming.
    let cold_probe = gen_query(&mut rng, &args);
    let cold_start = Instant::now();
    let cold_result = client.query(&args.namespace, &cold_probe).await;
    let first_request_ms = cold_start.elapsed().as_secs_f64() * 1000.0;
    if let Err(e) = &cold_result {
        anyhow::bail!("initial probe query failed: {e:#}");
    }

    if args.warm {
        client.warm_namespace(&args.namespace, None).await?;
    }

    let progress = if json_mode {
        None
    } else {
        let pb = ProgressBar::new(requests.len() as u64);
        pb.set_style(
            ProgressStyle::with_template("{bar:40} {pos}/{len} queries ({per_sec})")
                .expect("valid progress template"),
        );
        Some(pb)
    };

    let wall_start = Instant::now();
    let results: Vec<Result<u64>> = stream::iter(requests.into_iter().map(|req| {
        let client = client.clone();
        let namespace = args.namespace.clone();
        let progress = progress.clone();
        async move {
            let start = Instant::now();
            client.query(&namespace, &req).await?;
            if let Some(pb) = &progress {
                pb.inc(1);
            }
            Ok::<u64, anyhow::Error>(start.elapsed().as_micros() as u64)
        }
    }))
    .buffer_unordered(args.concurrency.max(1))
    .collect()
    .await;
    let wall = wall_start.elapsed();

    if let Some(pb) = &progress {
        pb.finish_and_clear();
    }

    let mut hist = Histogram::<u64>::new(3).context("creating latency histogram")?;
    let mut errors = 0usize;
    let mut first_error: Option<String> = None;
    for r in results {
        match r {
            Ok(micros) => hist.record(micros.max(1)).context("recording latency")?,
            Err(e) => {
                errors += 1;
                first_error.get_or_insert_with(|| format!("{e:#}"));
            }
        }
    }

    let succeeded = hist.len();
    let qps = succeeded as f64 / wall.as_secs_f64().max(1e-9);
    let to_ms = |micros: u64| micros as f64 / 1000.0;

    let mode = match args.mode {
        BenchQueryMode::Vector => "vector",
        BenchQueryMode::Text => "text",
        BenchQueryMode::Hybrid => "hybrid",
    };

    let report = json!({
        "benchmark": "query",
        "namespace": args.namespace,
        "mode": mode,
        "queries": succeeded,
        "errors": errors,
        "first_error": first_error,
        "concurrency": args.concurrency,
        "top_k": args.top_k,
        "warmed": args.warm,
        "first_request_ms": round2(first_request_ms),
        "qps": round2(qps),
        "latency_ms": {
            "mean": round2(hist.mean() / 1000.0),
            "p50": round2(to_ms(hist.value_at_quantile(0.50))),
            "p90": round2(to_ms(hist.value_at_quantile(0.90))),
            "p99": round2(to_ms(hist.value_at_quantile(0.99))),
            "max": round2(to_ms(hist.max())),
        },
    });

    if json_mode {
        output::print_json(&report);
    } else {
        println!("query benchmark ({mode}, top_k={}, concurrency={})", args.top_k, args.concurrency);
        println!("  first request (cold) : {:.2} ms", first_request_ms);
        if args.warm {
            println!("  cache warmed before measured run");
        }
        println!("  measured queries     : {succeeded} ({errors} errors)");
        println!("  throughput           : {:.1} qps", qps);
        println!(
            "  latency ms           : mean {:.2} | p50 {:.2} | p90 {:.2} | p99 {:.2} | max {:.2}",
            hist.mean() / 1000.0,
            to_ms(hist.value_at_quantile(0.50)),
            to_ms(hist.value_at_quantile(0.90)),
            to_ms(hist.value_at_quantile(0.99)),
            to_ms(hist.max()),
        );
        if let Some(e) = first_error {
            println!("  first error          : {e}");
        }
    }
    Ok(())
}

fn gen_query(rng: &mut StdRng, args: &QueryBenchArgs) -> QueryRequest {
    let vector = || -> Vec<f32> {
        let mut r = rng.clone();
        (0..args.dims).map(|_| r.gen::<f32>() * 2.0 - 1.0).collect()
    };
    let text = |rng: &mut StdRng| -> String {
        let n = rng.gen_range(2..4);
        (0..n)
            .map(|_| WORDS[rng.gen_range(0..WORDS.len())])
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut req = QueryRequest {
        top_k: args.top_k,
        include_attributes: Some(serde_json::Value::Bool(false)),
        ..Default::default()
    };
    match args.mode {
        BenchQueryMode::Vector => {
            req.vector = Some((0..args.dims).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect());
        }
        BenchQueryMode::Text => {
            req.text = Some(text(rng));
        }
        BenchQueryMode::Hybrid => {
            req.vector = Some(vector());
            req.text = Some(text(rng));
            req.fusion = Some(Fusion {
                method: "rrf".to_string(),
                vector_weight: None,
                text_weight: None,
            });
        }
    }
    req
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_doc_is_deterministic_per_seed() {
        let mut a = StdRng::seed_from_u64(1);
        let mut b = StdRng::seed_from_u64(1);
        let d1 = gen_doc(&mut a, 0, 8, true);
        let d2 = gen_doc(&mut b, 0, 8, true);
        assert_eq!(d1.vector, d2.vector);
        assert_eq!(d1.attributes, d2.attributes);
    }

    #[test]
    fn doc_byte_estimate_counts_vector() {
        let mut rng = StdRng::seed_from_u64(2);
        let d = gen_doc(&mut rng, 0, 16, false);
        assert!(estimate_doc_bytes(&d) >= 16 * 4);
    }

    #[test]
    fn round2_rounds() {
        assert_eq!(round2(1.005), 1.01);
        assert_eq!(round2(2.0), 2.0);
    }
}
