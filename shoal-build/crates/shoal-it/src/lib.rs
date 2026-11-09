//! Integration-test harness for Shoal.
//!
//! This crate spins up a real `shoal-server` in-process (real router, real
//! auth middleware, real engine, real storage backend) on an ephemeral TCP
//! port, and drives it over HTTP exactly like an external client would.
//!
//! ## Contract with `shoal-server`
//!
//! The harness depends only on the *public* surface of the server crate.
//! If any of these drift, fix them here (one place) rather than in the tests:
//!
//! 1. `shoal_server::config::ServerConfig` implements `serde::Deserialize`
//!    and can be parsed from the TOML rendered by [`render_config`].
//! 2. `shoal_server::build_app(ServerConfig) -> anyhow::Result<axum::Router>`
//!    constructs the fully-wired router (auth, metrics, audit, engine).
//! 3. HTTP routes are those listed in the [`api`] module.
//! 4. JSON request/response shapes are those produced by the builders in
//!    [`filters`], [`queries`], and [`corpus`] / [`doc`].
//!
//! Response parsing is deliberately tolerant (hits may live under `results`,
//! `matches`, `hits`, or `documents`; export may be a JSON array, an object
//! wrapper, or NDJSON) so cosmetic response-shape changes do not invalidate
//! the behavioural assertions the tests actually care about.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;

/// API keys provisioned into every test server, one per role.
pub const ADMIN_KEY: &str = "it-admin-key-000000000000";
pub const WRITER_KEY: &str = "it-writer-key-00000000000";
pub const READER_KEY: &str = "it-reader-key-00000000000";

/// Vector dimensionality used by the standard test corpus.
pub const DIM: usize = 8;

/// Route table. Single source of truth for every path the tests touch.
pub mod api {
    pub const HEALTH: &str = "/healthz";
    pub const METRICS: &str = "/metrics";

    pub fn namespaces() -> String {
        "/v1/namespaces".to_string()
    }
    pub fn namespace(ns: &str) -> String {
        format!("/v1/namespaces/{ns}")
    }
    pub fn documents(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/documents")
    }
    pub fn documents_delete(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/documents/delete")
    }
    pub fn query(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/query")
    }
    pub fn export(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/export")
    }
    pub fn copy(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/copy")
    }
    pub fn branch(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/branch")
    }
    pub fn warm(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/warm")
    }
    pub fn pin(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/pin")
    }
    pub fn unpin(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/unpin")
    }
    pub fn compact(ns: &str) -> String {
        format!("/v1/namespaces/{ns}/compact")
    }
}

/// Filter-expression builders matching the JSON filter grammar.
pub mod filters {
    use serde_json::{json, Value};

    pub fn eq(field: &str, value: Value) -> Value {
        json!({"op": "eq", "field": field, "value": value})
    }
    pub fn not_eq(field: &str, value: Value) -> Value {
        json!({"op": "not_eq", "field": field, "value": value})
    }
    pub fn gt(field: &str, value: Value) -> Value {
        json!({"op": "gt", "field": field, "value": value})
    }
    pub fn gte(field: &str, value: Value) -> Value {
        json!({"op": "gte", "field": field, "value": value})
    }
    pub fn lt(field: &str, value: Value) -> Value {
        json!({"op": "lt", "field": field, "value": value})
    }
    pub fn lte(field: &str, value: Value) -> Value {
        json!({"op": "lte", "field": field, "value": value})
    }
    pub fn r#in(field: &str, values: Value) -> Value {
        json!({"op": "in", "field": field, "values": values})
    }
    pub fn contains_any(field: &str, values: Value) -> Value {
        json!({"op": "contains_any", "field": field, "values": values})
    }
    pub fn and(clauses: Vec<Value>) -> Value {
        json!({"op": "and", "filters": clauses})
    }
    pub fn or(clauses: Vec<Value>) -> Value {
        json!({"op": "or", "filters": clauses})
    }
    pub fn not(clause: Value) -> Value {
        json!({"op": "not", "filter": clause})
    }
}

/// Query-body builders.
pub mod queries {
    use serde_json::{json, Value};

    pub fn vector(values: &[f32], metric: &str, top_k: u32) -> Value {
        json!({
            "top_k": top_k,
            "vector": {"values": values, "metric": metric}
        })
    }

    pub fn text(query: &str, top_k: u32) -> Value {
        json!({
            "top_k": top_k,
            "text": {"query": query}
        })
    }

    pub fn hybrid(values: &[f32], metric: &str, query: &str, fusion: Value, top_k: u32) -> Value {
        json!({
            "top_k": top_k,
            "vector": {"values": values, "metric": metric},
            "text": {"query": query},
            "fusion": fusion
        })
    }

    pub fn rrf() -> Value {
        json!({"method": "rrf"})
    }

    pub fn weighted(vector_weight: f64, text_weight: f64) -> Value {
        json!({
            "method": "weighted",
            "vector_weight": vector_weight,
            "text_weight": text_weight
        })
    }

    pub fn with_filter(mut q: Value, filter: Value) -> Value {
        q["filter"] = filter;
        q
    }

    pub fn with_include(mut q: Value, attrs: &[&str]) -> Value {
        q["include_attributes"] = json!(attrs);
        q
    }
}

// ---------------------------------------------------------------------------
// Backend specification & config rendering
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct S3Opts {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

#[derive(Clone, Debug)]
pub enum BackendSpec {
    Local { root: PathBuf },
    S3(S3Opts),
}

/// Read MinIO/S3 connection details from the environment, or `None` if the
/// suite should skip S3-backed tests.
///
/// Variables:
/// - `SHOAL_IT_S3_ENDPOINT`   (required to enable; e.g. `http://127.0.0.1:9000`)
/// - `SHOAL_IT_S3_BUCKET`     (default `shoal-it`; must already exist)
/// - `SHOAL_IT_S3_ACCESS_KEY` (default `minioadmin`)
/// - `SHOAL_IT_S3_SECRET_KEY` (default `minioadmin`)
/// - `SHOAL_IT_S3_REGION`     (default `us-east-1`)
pub fn s3_opts_from_env() -> Option<S3Opts> {
    let endpoint = std::env::var("SHOAL_IT_S3_ENDPOINT").ok()?;
    if endpoint.trim().is_empty() {
        return None;
    }
    Some(S3Opts {
        endpoint,
        bucket: std::env::var("SHOAL_IT_S3_BUCKET").unwrap_or_else(|_| "shoal-it".into()),
        prefix: format!("it-{}", short_id()),
        region: std::env::var("SHOAL_IT_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key: std::env::var("SHOAL_IT_S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into()),
        secret_key: std::env::var("SHOAL_IT_S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into()),
    })
}

pub fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Generate a unique namespace name for a test.
pub fn ns_name(prefix: &str) -> String {
    format!("{prefix}-{}", short_id())
}

fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn render_config(spec: &BackendSpec, cache_dir: &Path) -> String {
    let storage = match spec {
        BackendSpec::Local { root } => format!(
            r#"[storage]
backend = "local"
root = "{root}"
"#,
            root = toml_path(root)
        ),
        BackendSpec::S3(s3) => format!(
            r#"[storage]
backend = "s3"

[storage.s3]
endpoint = "{endpoint}"
bucket = "{bucket}"
prefix = "{prefix}"
region = "{region}"
access_key_id = "{ak}"
secret_access_key = "{sk}"
force_path_style = true
"#,
            endpoint = s3.endpoint,
            bucket = s3.bucket,
            prefix = s3.prefix,
            region = s3.region,
            ak = s3.access_key,
            sk = s3.secret_key,
        ),
    };

    format!(
        r#"listen_addr = "127.0.0.1:0"

{storage}
[cache]
disk_dir = "{cache}"
disk_capacity_bytes = 268435456
memory_capacity_bytes = 67108864

[auth]
keys = [
  {{ key = "{admin}", role = "admin" }},
  {{ key = "{writer}", role = "writer" }},
  {{ key = "{reader}", role = "reader" }},
]

[rate_limit]
enabled = false

[audit]
enabled = true
"#,
        storage = storage,
        cache = toml_path(cache_dir),
        admin = ADMIN_KEY,
        writer = WRITER_KEY,
        reader = READER_KEY,
    )
}

// ---------------------------------------------------------------------------
// TestServer
// ---------------------------------------------------------------------------

pub struct TestServer {
    pub base_url: String,
    pub spec: BackendSpec,
    pub cache_dir: PathBuf,
    http: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
    _tempdirs: Vec<TempDir>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestServer {
    /// Spawn a server backed by a fresh local-filesystem object store.
    pub async fn spawn_local() -> Result<Self> {
        let root = TempDir::new().context("create storage tempdir")?;
        let spec = BackendSpec::Local {
            root: root.path().to_path_buf(),
        };
        Self::spawn_inner(spec, vec![root]).await
    }

    /// Spawn a server backed by S3/MinIO (bucket must exist; unique prefix).
    pub async fn spawn_s3(opts: S3Opts) -> Result<Self> {
        Self::spawn_inner(BackendSpec::S3(opts), Vec::new()).await
    }

    /// Stop this server and start a fresh one against the *same* durable
    /// storage, but with brand-new (empty) caches. This is the canonical
    /// "cold restart" used by recovery and cold-query tests.
    pub async fn restart_cold(mut self) -> Result<Self> {
        self.task.abort();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let spec = self.spec.clone();
        let dirs = std::mem::take(&mut self._tempdirs);
        Self::spawn_inner(spec, dirs).await
    }

    async fn spawn_inner(spec: BackendSpec, mut dirs: Vec<TempDir>) -> Result<Self> {
        let cache = TempDir::new().context("create cache tempdir")?;
        let cache_dir = cache.path().to_path_buf();
        dirs.push(cache);

        let cfg_toml = render_config(&spec, &cache_dir);
        let config: shoal_server::config::ServerConfig = toml::from_str(&cfg_toml)
            .with_context(|| format!("parse test ServerConfig:\n{cfg_toml}"))?;

        let app = shoal_server::build_app(config)
            .await
            .map_err(|e| anyhow!("build_app failed: {e:#}"))?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind ephemeral port")?;
        let addr = listener.local_addr()?;

        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("shoal-it: server exited: {e}");
            }
        });

        let base_url = format!("http://{addr}");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build reqwest client")?;

        let server = TestServer {
            base_url,
            spec,
            cache_dir,
            http,
            task,
            _tempdirs: dirs,
        };
        server.wait_healthy().await?;
        Ok(server)
    }

    async fn wait_healthy(&self) -> Result<()> {
        for _ in 0..100 {
            if let Ok(resp) = self
                .http
                .get(format!("{}{}", self.base_url, api::HEALTH))
                .send()
                .await
            {
                if resp.status().is_success() {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("server did not become healthy within 10s")
    }

    // -- raw HTTP -----------------------------------------------------------

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base_url, path));
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return (0, json!({"transport_error": e.to_string()})),
        };
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let parsed = serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|_| json!({"raw": text}));
        (status, parsed)
    }

    pub async fn get(&self, path: &str, key: Option<&str>) -> (u16, Value) {
        self.request(reqwest::Method::GET, path, key, None).await
    }

    pub async fn post(&self, path: &str, key: Option<&str>, body: Value) -> (u16, Value) {
        self.request(reqwest::Method::POST, path, key, Some(body)).await
    }

    pub async fn patch(&self, path: &str, key: Option<&str>, body: Value) -> (u16, Value) {
        self.request(reqwest::Method::PATCH, path, key, Some(body)).await
    }

    pub async fn delete(&self, path: &str, key: Option<&str>) -> (u16, Value) {
        self.request(reqwest::Method::DELETE, path, key, None).await
    }

    pub async fn get_text(&self, path: &str, key: Option<&str>) -> (u16, String) {
        let mut req = self.http.get(format!("{}{}", self.base_url, path));
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        match req.send().await {
            Ok(r) => {
                let status = r.status().as_u16();
                (status, r.text().await.unwrap_or_default())
            }
            Err(e) => (0, format!("transport error: {e}")),
        }
    }

    // -- domain helpers -----------------------------------------------------

    pub async fn create_namespace(&self, ns: &str) -> Result<Value> {
        let (status, body) = self
            .post(&api::namespaces(), Some(ADMIN_KEY), json!({"name": ns}))
            .await;
        ensure_2xx(status, &body, &format!("create namespace {ns}"))?;
        Ok(body)
    }

    pub async fn delete_namespace(&self, ns: &str) -> Result<Value> {
        let (status, body) = self.delete(&api::namespace(ns), Some(ADMIN_KEY)).await;
        ensure_2xx(status, &body, &format!("delete namespace {ns}"))?;
        Ok(body)
    }

    pub async fn upsert(&self, ns: &str, docs: &[Value]) -> Result<Value> {
        let (status, body) = self
            .post(
                &api::documents(ns),
                Some(WRITER_KEY),
                json!({"documents": docs}),
            )
            .await;
        ensure_2xx(status, &body, &format!("upsert into {ns}"))?;
        Ok(body)
    }

    pub async fn patch_docs(&self, ns: &str, docs: &[Value]) -> Result<Value> {
        let (status, body) = self
            .patch(
                &api::documents(ns),
                Some(WRITER_KEY),
                json!({"documents": docs}),
            )
            .await;
        ensure_2xx(status, &body, &format!("patch in {ns}"))?;
        Ok(body)
    }

    pub async fn delete_by_ids(&self, ns: &str, ids: &[&str]) -> Result<Value> {
        let (status, body) = self
            .post(
                &api::documents_delete(ns),
                Some(WRITER_KEY),
                json!({"ids": ids}),
            )
            .await;
        ensure_2xx(status, &body, &format!("delete by ids in {ns}"))?;
        Ok(body)
    }

    pub async fn delete_by_filter(&self, ns: &str, filter: Value) -> Result<Value> {
        let (status, body) = self
            .post(
                &api::documents_delete(ns),
                Some(WRITER_KEY),
                json!({"filter": filter}),
            )
            .await;
        ensure_2xx(status, &body, &format!("delete by filter in {ns}"))?;
        Ok(body)
    }

    pub async fn query(&self, ns: &str, body: Value) -> Result<Value> {
        let (status, resp) = self.post(&api::query(ns), Some(READER_KEY), body).await;
        ensure_2xx(status, &resp, &format!("query {ns}"))?;
        Ok(resp)
    }

    /// Run a query and return the hit IDs in rank order.
    pub async fn top_ids(&self, ns: &str, body: Value) -> Result<Vec<String>> {
        let resp = self.query(ns, body).await?;
        Ok(extract_hits(&resp)
            .iter()
            .filter_map(hit_id)
            .collect())
    }

    /// Export a namespace and return its documents.
    pub async fn export_docs(&self, ns: &str) -> Result<Vec<Value>> {
        let (status, text) = self.get_text(&api::export(ns), Some(READER_KEY)).await;
        if !(200..300).contains(&(status as i32)) {
            bail!("export {ns}: status {status}: {text}");
        }
        Ok(parse_export(&text))
    }

    /// Export a namespace and return the sorted set of document IDs.
    pub async fn export_ids(&self, ns: &str) -> Result<Vec<String>> {
        let docs = self.export_docs(ns).await?;
        let mut ids: Vec<String> = docs.iter().filter_map(hit_id).collect();
        ids.sort();
        Ok(ids)
    }

    /// Branch `source` into a new namespace `target`.
    pub async fn branch(&self, source: &str, target: &str) -> Result<Value> {
        let (status, body) = self
            .post(&api::branch(source), Some(ADMIN_KEY), json!({"target": target}))
            .await;
        ensure_2xx(status, &body, &format!("branch {source} -> {target}"))?;
        Ok(body)
    }

    /// Deep-copy `source` into a new namespace `target`.
    pub async fn copy(&self, source: &str, target: &str) -> Result<Value> {
        let (status, body) = self
            .post(&api::copy(source), Some(ADMIN_KEY), json!({"target": target}))
            .await;
        ensure_2xx(status, &body, &format!("copy {source} -> {target}"))?;
        Ok(body)
    }

    pub async fn warm(&self, ns: &str) -> Result<Value> {
        let (status, body) = self.post(&api::warm(ns), Some(ADMIN_KEY), json!({})).await;
        ensure_2xx(status, &body, &format!("warm {ns}"))?;
        Ok(body)
    }

    /// Best-effort compaction trigger; ignored if the route is absent.
    pub async fn try_compact(&self, ns: &str) {
        let _ = self.post(&api::compact(ns), Some(ADMIN_KEY), json!({})).await;
    }

    pub async fn pin(&self, ns: &str) -> Result<Value> {
        let (status, body) = self.post(&api::pin(ns), Some(ADMIN_KEY), json!({})).await;
        ensure_2xx(status, &body, &format!("pin {ns}"))?;
        Ok(body)
    }

    /// Unpin via `DELETE /pin`, falling back to `POST /unpin`.
    pub async fn unpin(&self, ns: &str) -> Result<()> {
        let (status, _) = self.delete(&api::pin(ns), Some(ADMIN_KEY)).await;
        if (200..300).contains(&(status as i32)) {
            return Ok(());
        }
        let (status, body) = self.post(&api::unpin(ns), Some(ADMIN_KEY), json!({})).await;
        ensure_2xx(status, &body, &format!("unpin {ns}"))
    }

    // -- observability helpers ----------------------------------------------

    pub async fn metrics_text(&self) -> String {
        let (_, text) = self.get_text(api::METRICS, None).await;
        text
    }

    /// Sum all Prometheus sample values whose metric line contains every
    /// needle (case-insensitive). Robust against exact metric naming.
    pub async fn metric_sum(&self, needles: &[&str]) -> f64 {
        let text = self.metrics_text().await;
        let mut sum = 0.0;
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if needles.iter().all(|n| lower.contains(&n.to_ascii_lowercase())) {
                if let Some(tok) = line.split_whitespace().last() {
                    if let Ok(v) = tok.parse::<f64>() {
                        sum += v;
                    }
                }
            }
        }
        sum
    }

    /// Count regular files anywhere under the disk cache directory.
    pub fn cache_file_count(&self) -> usize {
        fn walk(dir: &Path, count: &mut usize) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        walk(&p, count);
                    } else {
                        *count += 1;
                    }
                }
            }
        }
        let mut count = 0;
        walk(&self.cache_dir, &mut count);
        count
    }
}

// ---------------------------------------------------------------------------
// Corpus, parsing helpers, assertions
// ---------------------------------------------------------------------------

/// One-hot basis vector of dimension `dim` along `axis`.
pub fn basis(dim: usize, axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[axis % dim] = 1.0;
    v
}

/// Build a single document.
pub fn doc(id: &str, vector: Vec<f32>, title: &str, body: &str, extra: Value) -> Value {
    let mut attributes = json!({"title": title, "body": body});
    if let (Some(dst), Some(src)) = (attributes.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    json!({"id": id, "vector": vector, "attributes": attributes})
}

/// Standard deterministic corpus: `n <= DIM` documents with one-hot vectors,
/// a shared token `common`, a unique token `alpha{i}`, alternating
/// `lang` (`en`/`fr`), and numeric `rank = i`.
pub fn corpus(n: usize) -> Vec<Value> {
    assert!(n <= DIM, "standard corpus supports at most {DIM} docs");
    (0..n)
        .map(|i| {
            doc(
                &format!("doc-{i}"),
                basis(DIM, i),
                &format!("Title {i}"),
                &format!("common alpha{i} body text for document {i}"),
                json!({
                    "lang": if i % 2 == 0 { "en" } else { "fr" },
                    "rank": i
                }),
            )
        })
        .collect()
}

/// Create a namespace and seed it with the standard corpus.
pub async fn seed(srv: &TestServer, ns: &str, n: usize) -> Result<()> {
    srv.create_namespace(ns).await?;
    srv.upsert(ns, &corpus(n)).await?;
    Ok(())
}

/// Find the hits array inside a query response, tolerating key drift.
pub fn extract_hits(v: &Value) -> Vec<Value> {
    for key in ["results", "matches", "hits", "documents"] {
        if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
            return arr.clone();
        }
    }
    if let Some(arr) = v.as_array() {
        return arr.clone();
    }
    Vec::new()
}

/// Extract a document/hit ID, tolerating nesting under `document`.
pub fn hit_id(h: &Value) -> Option<String> {
    let direct = h.get("id");
    let nested = h.get("document").and_then(|d| d.get("id"));
    match direct.or(nested)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Get an attribute from an exported document or query hit, tolerating
/// flat vs. nested-`attributes` layouts.
pub fn doc_attr<'a>(d: &'a Value, key: &str) -> Option<&'a Value> {
    d.get("attributes")
        .and_then(|a| a.get(key))
        .or_else(|| d.get("document").and_then(|x| x.get("attributes")).and_then(|a| a.get(key)))
        .or_else(|| d.get(key))
}

/// Parse an export body: JSON array, `{documents: [...]}`-style wrapper,
/// or NDJSON lines.
pub fn parse_export(text: &str) -> Vec<Value> {
    let t = text.trim();
    if t.is_empty() {
        return Vec::new();
    }
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        if let Some(arr) = v.as_array() {
            return arr.clone();
        }
        if let Some(obj) = v.as_object() {
            for key in ["documents", "docs", "rows", "items"] {
                if let Some(arr) = obj.get(key).and_then(|x| x.as_array()) {
                    return arr.clone();
                }
            }
        }
    }
    t.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .collect()
}

pub fn ensure_2xx(status: u16, body: &Value, ctx: &str) -> Result<()> {
    if (200..300).contains(&(status as i32)) {
        Ok(())
    } else {
        bail!("{ctx}: expected 2xx, got {status}: {body}")
    }
}

/// Assert that the namespace's exported ID set matches `expected` exactly.
pub async fn assert_ids(srv: &TestServer, ns: &str, expected: &[&str]) -> Result<()> {
    let actual = srv.export_ids(ns).await?;
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    if actual != want {
        bail!("namespace {ns}: id mismatch\n  expected: {want:?}\n  actual:   {actual:?}");
    }
    Ok(())
}
