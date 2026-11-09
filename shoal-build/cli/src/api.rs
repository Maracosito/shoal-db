//! Minimal typed HTTP client for the Lakefin API, used by every CLI command.
//!
//! Retries are applied automatically for transient failures (connect errors,
//! timeouts, HTTP 429/502/503/504) with exponential backoff and jitter.
//! Upserts are idempotent on the server (keyed by document ID plus an optional
//! `Idempotency-Key` header), so retrying writes is safe.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const MAX_RETRIES: u32 = 3;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fusion {
    /// "rrf" (reciprocal rank fusion) or "weighted" (normalized score fusion)
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_weight: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_weight: Option<f32>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryRequest {
    pub top_k: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_vector: Option<SparseVector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// `true` for all attributes, or an array of attribute names
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_attributes: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    pub include_vectors: bool,
    /// "auto" | "exact" | "ann"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fusion: Option<Fusion>,
}

impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            top_k: 10,
            vector: None,
            text: None,
            sparse_vector: None,
            filter: None,
            include_attributes: Some(Value::Bool(true)),
            include_vectors: false,
            mode: None,
            fusion: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryResult {
    pub id: String,
    pub score: f64,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryResponse {
    pub results: Vec<QueryResult>,
    #[serde(default)]
    pub took_ms: Option<f64>,
    #[serde(default)]
    pub plan: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpsertResponse {
    #[serde(default)]
    pub upserted: Option<u64>,
    #[serde(default)]
    pub wal_offset: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportPage {
    pub documents: Vec<Document>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
}

impl ApiClient {
    pub fn new(base: &str, api_key: Option<String>, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("lakefin-cli/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            api_key,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        rb
    }

    async fn execute_with_retry(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let mut attempt: u32 = 0;
        loop {
            let req = rb
                .try_clone()
                .ok_or_else(|| anyhow!("internal: request body is not retryable"))?;
            let retryable = match req.send().await {
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    if matches!(code, 429 | 502 | 503 | 504) && attempt < MAX_RETRIES {
                        true
                    } else {
                        return Ok(resp);
                    }
                }
                Err(err) => {
                    if (err.is_connect() || err.is_timeout() || err.is_request())
                        && attempt < MAX_RETRIES
                    {
                        true
                    } else {
                        return Err(err).context("sending request");
                    }
                }
            };
            if retryable {
                attempt += 1;
                let jitter_ms: u64 = rand::thread_rng().gen_range(0..100);
                let backoff =
                    Duration::from_millis(250 * 2u64.pow(attempt.min(4)) + jitter_ms);
                tokio::time::sleep(backoff).await;
            }
        }
    }

    async fn send_json<T: DeserializeOwned>(&self, rb: reqwest::RequestBuilder) -> Result<T> {
        let resp = self.execute_with_retry(rb).await?;
        let status = resp.status();
        let body = resp.text().await.context("reading response body")?;
        if !status.is_success() {
            bail!(format_api_error(status.as_u16(), &body));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("decoding response body: {:.200}", body))
    }

    async fn send_unit(&self, rb: reqwest::RequestBuilder) -> Result<()> {
        let resp = self.execute_with_retry(rb).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!(format_api_error(status.as_u16(), &body));
        }
        Ok(())
    }

    // -- system ------------------------------------------------------------

    pub async fn health(&self) -> Result<Value> {
        self.send_json(self.request(reqwest::Method::GET, "/v1/health"))
            .await
    }

    pub async fn metrics(&self) -> Result<String> {
        let resp = self
            .execute_with_retry(self.request(reqwest::Method::GET, "/metrics"))
            .await?;
        let status = resp.status();
        let body = resp.text().await.context("reading metrics body")?;
        if !status.is_success() {
            bail!(format_api_error(status.as_u16(), &body));
        }
        Ok(body)
    }

    // -- namespaces ----------------------------------------------------------

    pub async fn create_namespace(
        &self,
        name: &str,
        dimensions: Option<usize>,
        distance_metric: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({ "name": name });
        if let Some(d) = dimensions {
            body["dimensions"] = json!(d);
        }
        if let Some(m) = distance_metric {
            body["distance_metric"] = json!(m);
        }
        self.send_json(
            self.request(reqwest::Method::POST, "/v1/namespaces")
                .json(&body),
        )
        .await
    }

    pub async fn list_namespaces(&self) -> Result<Value> {
        self.send_json(self.request(reqwest::Method::GET, "/v1/namespaces"))
            .await
    }

    pub async fn namespace_info(&self, ns: &str) -> Result<Value> {
        self.send_json(
            self.request(reqwest::Method::GET, &format!("/v1/namespaces/{}", enc(ns))),
        )
        .await
    }

    pub async fn delete_namespace(&self, ns: &str) -> Result<()> {
        self.send_unit(
            self.request(
                reqwest::Method::DELETE,
                &format!("/v1/namespaces/{}", enc(ns)),
            ),
        )
        .await
    }

    pub async fn copy_namespace(&self, source: &str, target: &str) -> Result<Value> {
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/copy", enc(source)),
            )
            .json(&json!({ "target": target })),
        )
        .await
    }

    pub async fn branch_namespace(&self, source: &str, target: &str) -> Result<Value> {
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/branch", enc(source)),
            )
            .json(&json!({ "target": target })),
        )
        .await
    }

    /// Warm a namespace's caches. `pin = Some(true)` keeps it resident,
    /// `Some(false)` removes any pin, `None` warms without changing pinning.
    pub async fn warm_namespace(&self, ns: &str, pin: Option<bool>) -> Result<Value> {
        let body = match pin {
            Some(p) => json!({ "pin": p }),
            None => json!({}),
        };
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/warm", enc(ns)),
            )
            .json(&body),
        )
        .await
    }

    // -- documents -----------------------------------------------------------

    pub async fn upsert(
        &self,
        ns: &str,
        documents: &[Document],
        idempotency_key: Option<&str>,
    ) -> Result<UpsertResponse> {
        let mut rb = self
            .request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/documents", enc(ns)),
            )
            .json(&json!({ "documents": documents }));
        if let Some(key) = idempotency_key {
            rb = rb.header("Idempotency-Key", key);
        }
        self.send_json(rb).await
    }

    pub async fn delete_by_ids(&self, ns: &str, ids: &[String]) -> Result<Value> {
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/documents/delete", enc(ns)),
            )
            .json(&json!({ "ids": ids })),
        )
        .await
    }

    pub async fn delete_by_filter(&self, ns: &str, filter: &Value) -> Result<Value> {
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/documents/delete", enc(ns)),
            )
            .json(&json!({ "filter": filter })),
        )
        .await
    }

    // -- query / export -------------------------------------------------------

    pub async fn query(&self, ns: &str, req: &QueryRequest) -> Result<QueryResponse> {
        self.send_json(
            self.request(
                reqwest::Method::POST,
                &format!("/v1/namespaces/{}/query", enc(ns)),
            )
            .json(req),
        )
        .await
    }

    pub async fn export_page(
        &self,
        ns: &str,
        cursor: Option<&str>,
        limit: usize,
        include_vectors: bool,
    ) -> Result<ExportPage> {
        let mut path = format!(
            "/v1/namespaces/{}/export?limit={}&include_vectors={}",
            enc(ns),
            limit,
            include_vectors
        );
        if let Some(c) = cursor {
            path.push_str(&format!("&cursor={}", enc(c)));
        }
        self.send_json(self.request(reqwest::Method::GET, &path))
            .await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_api_error(status: u16, body: &str) -> String {
    // Server errors look like {"error": {"code": "...", "message": "..."}}
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str().map(String::from))
        })
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                "(no response body)".to_string()
            } else {
                format!("{:.300}", trimmed)
            }
        });
    format!("HTTP {status}: {detail}")
}

/// Percent-encode a single URL path segment.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_passes_safe_chars() {
        assert_eq!(enc("my-namespace_1.0~x"), "my-namespace_1.0~x");
    }

    #[test]
    fn enc_escapes_unsafe_chars() {
        assert_eq!(enc("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn api_error_extracts_nested_message() {
        let msg = format_api_error(404, r#"{"error":{"code":"not_found","message":"namespace missing"}}"#);
        assert_eq!(msg, "HTTP 404: namespace missing");
    }

    #[test]
    fn api_error_falls_back_to_body() {
        let msg = format_api_error(500, "boom");
        assert_eq!(msg, "HTTP 500: boom");
    }

    #[test]
    fn query_request_serializes_minimal() {
        let req = QueryRequest {
            top_k: 5,
            text: Some("hello".into()),
            include_attributes: None,
            ..Default::default()
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["top_k"], 5);
        assert_eq!(v["text"], "hello");
        assert!(v.get("vector").is_none());
        assert!(v.get("include_vectors").is_none());
        assert!(v.get("fusion").is_none());
    }
}
