//! `reef` — administration and data CLI for the Reef storage engine.
//!
//! This binary speaks directly to object storage (no API server required),
//! which makes it suitable for operational tasks: verifying namespace
//! integrity, rebuilding derived state, repairing after crashes, compacting,
//! garbage-collecting, and bulk-loading data during development.
//!
//! Examples:
//!
//! ```text
//! # local filesystem store
//! reef --store .reef-data -n docs init
//! reef --store .reef-data -n docs upsert --file corpus.jsonl
//! reef --store .reef-data -n docs index
//! reef --store .reef-data -n docs verify --deep
//!
//! # MinIO / S3
//! export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
//! export REEF_S3_ENDPOINT=http://localhost:9000
//! reef --store s3://reef-dev/tenant1 -n docs stats
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reef_engine::{admin, Engine, EngineOptions};
use reef_store::{store_from_url, ObjectStore};
use reef_types::document::{DocId, Document};
use reef_types::write::{ColumnarBatch, Patch, WriteBatch, WriteOp};

/// Root prefix under which namespaces live in the store layout.
/// Mirrors `reef_engine::layout`.
const NAMESPACES_PREFIX: &str = "namespaces/";

#[derive(Parser)]
#[command(
    name = "reef",
    version,
    about = "Reef — object-storage-native search database (storage engine CLI)",
    propagate_version = true
)]
struct Cli {
    /// Object store URL: `mem://`, `file://<path>`, a bare path, or
    /// `s3://bucket/prefix` (credentials/endpoint from environment).
    #[arg(long, global = true, env = "REEF_STORE", default_value = ".reef-data")]
    store: String,

    /// Namespace to operate on.
    #[arg(long, short = 'n', global = true, env = "REEF_NAMESPACE", default_value = "default")]
    namespace: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialise a namespace (creates the initial manifest if missing).
    Init,

    /// List namespaces present in the store.
    Namespaces,

    /// Upsert documents from a JSON array, JSONL file, or columnar JSON batch.
    Upsert {
        /// Input file. `.json` containing an array, JSONL otherwise, or a
        /// single column-oriented batch object with `--columnar`.
        #[arg(long)]
        file: PathBuf,
        /// Documents per write batch.
        #[arg(long, default_value_t = 500)]
        batch_size: usize,
        /// Idempotency key prefix; each batch is keyed `<prefix>:<batch-index>`
        /// so re-running the same load is deduplicated by the engine.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Treat the input as one column-oriented batch (`ids` + parallel columns).
        #[arg(long)]
        columnar: bool,
    },

    /// Apply partial updates from a JSONL file of patch objects.
    PatchDocs {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        idempotency_key: Option<String>,
    },

    /// Delete documents by id (comma-separated or repeated).
    Delete {
        #[arg(long, value_delimiter = ',', required = true)]
        ids: Vec<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },

    /// Fetch a single document by id and print it as JSON.
    Get {
        #[arg(long)]
        id: String,
    },

    /// Run one indexing pass: fold pending WAL entries into immutable segments.
    Index,

    /// Run one compaction pass: merge small segments into larger ones.
    Compact,

    /// Verify namespace integrity: manifest chain, WAL framing, segment
    /// checksums, and cross-references. Exits non-zero on problems.
    Verify {
        /// Also re-checksum every segment block (reads all data).
        #[arg(long)]
        deep: bool,
    },

    /// Rebuild derived state (segments and their indexes) from object storage
    /// alone, proving the namespace is reconstructable.
    Rebuild,

    /// Repair a namespace: quarantine corrupt/incomplete objects and roll the
    /// manifest back to the last consistent publish. Dry-run by default.
    Repair {
        /// Actually apply fixes (default is a dry run that only reports).
        #[arg(long)]
        apply: bool,
    },

    /// Garbage-collect unreferenced WAL/segment objects past the safety grace
    /// period. Dry-run by default.
    Gc {
        /// Actually delete (default is a dry run that only reports).
        #[arg(long)]
        apply: bool,
    },

    /// Print engine statistics for the namespace (segments, WAL backlog, sizes).
    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let store = store_from_url(&cli.store)
        .with_context(|| format!("failed to open object store `{}`", cli.store))?;
    let ns = cli.namespace.clone();

    match cli.command {
        Command::Init => {
            let _engine = open_engine(store.clone(), &ns).await?;
            println!("namespace `{ns}` is initialised");
        }

        Command::Namespaces => {
            let metas = store
                .list(NAMESPACES_PREFIX)
                .await
                .context("failed to list store")?;
            let mut names: BTreeSet<String> = BTreeSet::new();
            for meta in metas {
                if let Some(rest) = meta.key.strip_prefix(NAMESPACES_PREFIX) {
                    if let Some((name, _)) = rest.split_once('/') {
                        names.insert(name.to_string());
                    }
                }
            }
            if names.is_empty() {
                println!("(no namespaces)");
            }
            for name in names {
                println!("{name}");
            }
        }

        Command::Upsert { file, batch_size, idempotency_key, columnar } => {
            let docs = read_documents(&file, columnar)?;
            anyhow::ensure!(!docs.is_empty(), "no documents found in {}", file.display());
            anyhow::ensure!(batch_size > 0, "--batch-size must be > 0");
            let engine = open_engine(store.clone(), &ns).await?;
            let started = Instant::now();
            let mut batches = 0usize;
            let mut total = 0usize;
            for (i, chunk) in docs.chunks(batch_size).enumerate() {
                let ops: Vec<WriteOp> = chunk.iter().cloned().map(WriteOp::Upsert).collect();
                let mut batch = WriteBatch::new(ops);
                if let Some(prefix) = &idempotency_key {
                    batch = batch.with_idempotency_key(format!("{prefix}:{i}"));
                }
                engine
                    .apply(batch)
                    .await
                    .with_context(|| format!("failed to apply batch {i}"))?;
                batches += 1;
                total += chunk.len();
            }
            println!(
                "upserted {total} documents in {batches} batches in {:.2?}",
                started.elapsed()
            );
        }

        Command::PatchDocs { file, idempotency_key } => {
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let ops: Vec<WriteOp> = raw
                .lines()
                .enumerate()
                .filter(|(_, l)| !l.trim().is_empty())
                .map(|(i, l)| {
                    serde_json::from_str::<Patch>(l)
                        .with_context(|| format!("invalid patch on line {}", i + 1))
                        .map(WriteOp::Patch)
                })
                .collect::<Result<_>>()?;
            anyhow::ensure!(!ops.is_empty(), "no patches found in {}", file.display());
            let count = ops.len();
            let engine = open_engine(store.clone(), &ns).await?;
            let mut batch = WriteBatch::new(ops);
            if let Some(k) = idempotency_key {
                batch = batch.with_idempotency_key(k);
            }
            engine.apply(batch).await.context("failed to apply patches")?;
            println!("applied {count} patches");
        }

        Command::Delete { ids, idempotency_key } => {
            let ops: Vec<WriteOp> = ids
                .iter()
                .map(|raw| parse_doc_id(raw).map(WriteOp::Delete))
                .collect::<Result<_>>()?;
            let count = ops.len();
            let engine = open_engine(store.clone(), &ns).await?;
            let mut batch = WriteBatch::new(ops);
            if let Some(k) = idempotency_key {
                batch = batch.with_idempotency_key(k);
            }
            engine.apply(batch).await.context("failed to apply deletes")?;
            println!("deleted {count} document(s)");
        }

        Command::Get { id } => {
            let engine = open_engine(store.clone(), &ns).await?;
            let doc_id = parse_doc_id(&id)?;
            match engine.get(&doc_id).await.context("get failed")? {
                Some(doc) => print_json(&doc)?,
                None => println!("null"),
            }
        }

        Command::Index => {
            let engine = open_engine(store.clone(), &ns).await?;
            let started = Instant::now();
            let report = engine
                .run_indexer_once()
                .await
                .context("indexing pass failed")?;
            println!("{report:#?}");
            println!("indexing pass completed in {:.2?}", started.elapsed());
        }

        Command::Compact => {
            let engine = open_engine(store.clone(), &ns).await?;
            let started = Instant::now();
            let report = engine
                .run_compaction_once()
                .await
                .context("compaction pass failed")?;
            println!("{report:#?}");
            println!("compaction pass completed in {:.2?}", started.elapsed());
        }

        Command::Verify { deep } => {
            let report = admin::verify_namespace(store.clone(), &ns, deep)
                .await
                .context("verify failed to run")?;
            exit_by_report(&report)?;
        }

        Command::Rebuild => {
            let started = Instant::now();
            let report = admin::rebuild_namespace(store.clone(), &ns)
                .await
                .context("rebuild failed")?;
            print_json(&report)?;
            println!("rebuild completed in {:.2?}", started.elapsed());
        }

        Command::Repair { apply } => {
            let report = admin::repair_namespace(store.clone(), &ns, apply)
                .await
                .context("repair failed")?;
            if !apply {
                println!("(dry run — pass --apply to make changes)");
            }
            exit_by_report(&report)?;
        }

        Command::Gc { apply } => {
            let report = admin::gc_namespace(store.clone(), &ns, apply)
                .await
                .context("gc failed")?;
            if !apply {
                println!("(dry run — pass --apply to delete)");
            }
            print_json(&report)?;
        }

        Command::Stats => {
            let engine = open_engine(store.clone(), &ns).await?;
            let stats = engine.stats().await.context("failed to collect stats")?;
            print_json(&stats)?;
        }
    }

    Ok(())
}

/// Open (creating if necessary) the engine for a namespace.
async fn open_engine(store: Arc<dyn ObjectStore>, ns: &str) -> Result<Engine> {
    Engine::open(store, ns, EngineOptions::default())
        .await
        .with_context(|| format!("failed to open namespace `{ns}`"))
}

/// Parse a raw string into a `DocId` via its serde representation, so the CLI
/// stays agnostic of the id type's internals.
fn parse_doc_id(raw: &str) -> Result<DocId> {
    serde_json::from_value(serde_json::Value::String(raw.to_string()))
        .with_context(|| format!("invalid document id `{raw}`"))
}

/// Read documents from a JSON array file, a JSONL file, or a columnar batch.
fn read_documents(path: &PathBuf, columnar: bool) -> Result<Vec<Document>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if columnar {
        let batch: ColumnarBatch = serde_json::from_str(&raw)
            .with_context(|| format!("{} is not a valid columnar batch", path.display()))?;
        return batch
            .into_documents()
            .map_err(|e| anyhow::anyhow!("invalid columnar batch in {}: {e}", path.display()));
    }
    let trimmed = raw.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<Document>>(&raw)
            .with_context(|| format!("{} is not a valid JSON array of documents", path.display()));
    }
    raw.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str::<Document>(l)
                .with_context(|| format!("invalid document on line {} of {}", i + 1, path.display()))
        })
        .collect()
}

/// Pretty-print any serializable value as JSON.
fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print a report and exit with status 2 if it self-reports `"ok": false`.
fn exit_by_report<T: serde::Serialize>(report: &T) -> Result<()> {
    let value = serde_json::to_value(report)?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        std::process::exit(2);
    }
    Ok(())
}
