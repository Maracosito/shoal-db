use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::{json, Value};

use crate::api::{ApiClient, Document, SparseVector};

#[derive(Args)]
pub struct LoadArgs {
    /// Input file (.json array, .jsonl / .ndjson, or .csv)
    pub file: PathBuf,

    /// Target namespace
    #[arg(short = 'n', long)]
    pub namespace: String,

    /// Input format (inferred from the file extension when omitted)
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Field holding the document ID
    #[arg(long, default_value = "id")]
    pub id_field: String,

    /// Field holding the dense vector (JSON array, or "0.1,0.2,..." string)
    #[arg(long, default_value = "vector")]
    pub vector_field: String,

    /// Field holding a sparse vector object {"indices": [...], "values": [...]}
    #[arg(long, default_value = "sparse_vector")]
    pub sparse_field: String,

    /// Documents per upsert batch
    #[arg(long, default_value_t = 500)]
    pub batch_size: usize,

    /// Generate IDs ("row-<n>") for documents missing the ID field
    #[arg(long)]
    pub auto_id: bool,

    /// Create the namespace first if it does not exist
    #[arg(long)]
    pub create: bool,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum Format {
    Json,
    Jsonl,
    Csv,
}

pub async fn run(client: &ApiClient, args: LoadArgs, json_mode: bool) -> Result<()> {
    let format = match args.format {
        Some(f) => f,
        None => infer_format(&args.file)?,
    };

    if args.create {
        match client.create_namespace(&args.namespace, None, None).await {
            Ok(_) => {}
            // Namespace already exists -> fine.
            Err(e) if e.to_string().contains("HTTP 409") => {}
            Err(e) => return Err(e),
        }
    }

    let mut uploader = Uploader::new(
        client,
        &args.namespace,
        args.batch_size,
        idempotency_base(&args.file),
        !json_mode,
    );

    match format {
        Format::Jsonl => {
            let file = File::open(&args.file)
                .with_context(|| format!("opening {}", args.file.display()))?;
            let reader = BufReader::new(file);
            for (idx, line) in reader.lines().enumerate() {
                let line = line.context("reading line")?;
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(&line)
                    .with_context(|| format!("parsing JSON on line {}", idx + 1))?;
                let doc = value_to_document(value, idx, &args)?;
                uploader.push(doc).await?;
            }
        }
        Format::Json => {
            let raw = std::fs::read_to_string(&args.file)
                .with_context(|| format!("reading {}", args.file.display()))?;
            let value: Value = serde_json::from_str(&raw).context("parsing JSON file")?;
            let items = match value {
                Value::Array(items) => items,
                Value::Object(mut obj) => match obj.remove("documents") {
                    Some(Value::Array(items)) => items,
                    _ => bail!("JSON file must be an array or {{\"documents\": [...]}}"),
                },
                _ => bail!("JSON file must be an array or {{\"documents\": [...]}}"),
            };
            for (idx, item) in items.into_iter().enumerate() {
                let doc = value_to_document(item, idx, &args)?;
                uploader.push(doc).await?;
            }
        }
        Format::Csv => {
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(true)
                .from_path(&args.file)
                .with_context(|| format!("opening {}", args.file.display()))?;
            let headers: Vec<String> =
                reader.headers()?.iter().map(|h| h.to_string()).collect();
            for (idx, record) in reader.records().enumerate() {
                let record = record.with_context(|| format!("reading CSV row {}", idx + 2))?;
                let mut obj = serde_json::Map::new();
                for (header, field) in headers.iter().zip(record.iter()) {
                    if field.is_empty() {
                        continue;
                    }
                    obj.insert(header.clone(), infer_csv_value(field));
                }
                let doc = value_to_document(Value::Object(obj), idx, &args)?;
                uploader.push(doc).await?;
            }
        }
    }

    let total = uploader.finish().await?;
    if json_mode {
        println!(
            "{}",
            json!({ "loaded": total, "namespace": args.namespace })
        );
    } else {
        println!("loaded {total} documents into '{}'", args.namespace);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Batch uploader with progress reporting
// ---------------------------------------------------------------------------

struct Uploader<'a> {
    client: &'a ApiClient,
    namespace: &'a str,
    batch_size: usize,
    batch: Vec<Document>,
    batch_idx: u64,
    total: u64,
    key_base: String,
    progress: Option<ProgressBar>,
}

impl<'a> Uploader<'a> {
    fn new(
        client: &'a ApiClient,
        namespace: &'a str,
        batch_size: usize,
        key_base: String,
        show_progress: bool,
    ) -> Self {
        let progress = if show_progress {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .expect("valid progress template"),
            );
            Some(pb)
        } else {
            None
        };
        Self {
            client,
            namespace,
            batch_size: batch_size.max(1),
            batch: Vec::new(),
            batch_idx: 0,
            total: 0,
            key_base,
            progress,
        }
    }

    async fn push(&mut self, doc: Document) -> Result<()> {
        self.batch.push(doc);
        if self.batch.len() >= self.batch_size {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let key = format!("{}-{}", self.key_base, self.batch_idx);
        let batch = std::mem::take(&mut self.batch);
        let count = batch.len() as u64;
        self.client
            .upsert(self.namespace, &batch, Some(&key))
            .await
            .with_context(|| format!("upserting batch {}", self.batch_idx))?;
        self.batch_idx += 1;
        self.total += count;
        if let Some(pb) = &self.progress {
            pb.set_message(format!("{} documents loaded", self.total));
            pb.tick();
        }
        Ok(())
    }

    async fn finish(mut self) -> Result<u64> {
        self.flush().await?;
        if let Some(pb) = &self.progress {
            pb.finish_and_clear();
        }
        Ok(self.total)
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn infer_format(path: &Path) -> Result<Format> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("json") => Ok(Format::Json),
        Some("jsonl") | Some("ndjson") => Ok(Format::Jsonl),
        Some("csv") => Ok(Format::Csv),
        other => bail!(
            "cannot infer format from extension {:?}; pass --format json|jsonl|csv",
            other
        ),
    }
}

fn value_to_document(value: Value, idx: usize, args: &LoadArgs) -> Result<Document> {
    let Value::Object(mut map) = value else {
        bail!("document {} is not a JSON object", idx + 1);
    };

    let id = match map.remove(&args.id_field) {
        Some(Value::String(s)) => s,
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
        None if args.auto_id => format!("row-{idx}"),
        None => bail!(
            "document {} is missing id field '{}' (use --id-field or --auto-id)",
            idx + 1,
            args.id_field
        ),
    };

    let vector = match map.remove(&args.vector_field) {
        Some(v) => Some(parse_vector(&v).with_context(|| {
            format!("document '{id}': invalid vector in field '{}'", args.vector_field)
        })?),
        None => None,
    };

    let sparse_vector = match map.remove(&args.sparse_field) {
        Some(v) => Some(parse_sparse(&v).with_context(|| {
            format!(
                "document '{id}': invalid sparse vector in field '{}'",
                args.sparse_field
            )
        })?),
        None => None,
    };

    let attributes: BTreeMap<String, Value> = map.into_iter().collect();
    Ok(Document {
        id,
        vector,
        sparse_vector,
        attributes,
    })
}

pub fn parse_vector(v: &Value) -> Result<Vec<f32>> {
    match v {
        Value::Array(items) => items
            .iter()
            .map(|x| {
                x.as_f64()
                    .map(|f| f as f32)
                    .ok_or_else(|| anyhow::anyhow!("non-numeric vector element: {x}"))
            })
            .collect(),
        Value::String(s) => {
            let trimmed = s.trim();
            // A vector stored as a JSON-array string (common in CSV exports).
            if trimmed.starts_with('[') {
                let parsed: Value =
                    serde_json::from_str(trimmed).context("parsing vector JSON string")?;
                return parse_vector(&parsed);
            }
            let sep = if trimmed.contains(';') { ';' } else { ',' };
            trimmed
                .split(sep)
                .map(|p| {
                    p.trim()
                        .parse::<f32>()
                        .with_context(|| format!("parsing vector element '{p}'"))
                })
                .collect()
        }
        other => bail!("vector must be an array or delimited string, got: {other}"),
    }
}

fn parse_sparse(v: &Value) -> Result<SparseVector> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("sparse vector must be an object"))?;
    let indices = obj
        .get("indices")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow::anyhow!("sparse vector missing 'indices' array"))?
        .iter()
        .map(|x| {
            x.as_u64()
                .map(|u| u as u32)
                .ok_or_else(|| anyhow::anyhow!("non-integer sparse index: {x}"))
        })
        .collect::<Result<Vec<u32>>>()?;
    let values = obj
        .get("values")
        .and_then(|x| x.as_array())
        .ok_or_else(|| anyhow::anyhow!("sparse vector missing 'values' array"))?
        .iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| anyhow::anyhow!("non-numeric sparse value: {x}"))
        })
        .collect::<Result<Vec<f32>>>()?;
    if indices.len() != values.len() {
        bail!(
            "sparse vector indices ({}) and values ({}) length mismatch",
            indices.len(),
            values.len()
        );
    }
    Ok(SparseVector { indices, values })
}

pub fn infer_csv_value(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    match s {
        "true" | "TRUE" | "True" => return Value::Bool(true),
        "false" | "FALSE" | "False" => return Value::Bool(false),
        _ => {}
    }
    let trimmed = s.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            return v;
        }
    }
    Value::String(s.to_string())
}

/// Stable, content-independent base for batch idempotency keys
/// (FNV-1a over the absolute path so re-running the same load command
/// produces the same keys and the server can deduplicate).
fn idempotency_base(path: &Path) -> String {
    let canonical = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("lakefin-load-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_value_inference() {
        assert_eq!(infer_csv_value("42"), json!(42));
        assert_eq!(infer_csv_value("3.5"), json!(3.5));
        assert_eq!(infer_csv_value("true"), json!(true));
        assert_eq!(infer_csv_value("hello"), json!("hello"));
        assert_eq!(infer_csv_value("[1,2]"), json!([1, 2]));
    }

    #[test]
    fn vector_from_array() {
        let v = parse_vector(&json!([0.1, 0.2, 0.3])).unwrap();
        assert_eq!(v.len(), 3);
        assert!((v[1] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn vector_from_string_variants() {
        assert_eq!(parse_vector(&json!("1,2,3")).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(parse_vector(&json!("1;2;3")).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(
            parse_vector(&json!("[1.5, 2.5]")).unwrap(),
            vec![1.5, 2.5]
        );
    }

    #[test]
    fn sparse_round_trip() {
        let sv = parse_sparse(&json!({"indices": [1, 7], "values": [0.5, 0.25]})).unwrap();
        assert_eq!(sv.indices, vec![1, 7]);
        assert_eq!(sv.values, vec![0.5, 0.25]);
    }

    #[test]
    fn sparse_length_mismatch_rejected() {
        assert!(parse_sparse(&json!({"indices": [1], "values": [0.5, 0.25]})).is_err());
    }

    #[test]
    fn document_extraction_with_auto_id() {
        let args = LoadArgs {
            file: PathBuf::from("x.jsonl"),
            namespace: "ns".into(),
            format: None,
            id_field: "id".into(),
            vector_field: "vector".into(),
            sparse_field: "sparse_vector".into(),
            batch_size: 500,
            auto_id: true,
            create: false,
        };
        let doc =
            value_to_document(json!({"title": "no id here", "vector": [1.0]}), 3, &args).unwrap();
        assert_eq!(doc.id, "row-3");
        assert_eq!(doc.vector.as_deref(), Some(&[1.0][..]));
        assert_eq!(doc.attributes["title"], json!("no id here"));
    }
}
