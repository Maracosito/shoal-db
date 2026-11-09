//! S3-compatible [`ObjectStore`] backend.
//!
//! This is a deliberately dependency-light implementation: a hand-rolled AWS
//! Signature V4 signer over `reqwest`, rather than the full AWS SDK. It works
//! against AWS S3, MinIO, Cloudflare R2, and Ceph RGW.
//!
//! Notable behaviors:
//!
//! * `put_if_absent` is implemented with `If-None-Match: *`, which AWS S3
//!   (since August 2024) and MinIO both honor with `412 Precondition Failed`
//!   on conflict. This gives Reef its manifest compare-and-swap on plain S3.
//! * `get_range` uses HTTP `Range` requests so segment footers and individual
//!   blocks can be fetched without downloading whole segments.
//! * All requests are retried with capped exponential backoff + jitter on
//!   connect errors, timeouts, HTTP 5xx, and 429.
//! * Secrets are never logged; `S3Config`'s `Debug` impl redacts them.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

use crate::{ObjectMeta, ObjectStore, StoreError, StoreResult};

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 of the empty string, used as the payload hash for body-less requests.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Configuration for [`S3Store`].
#[derive(Clone)]
pub struct S3Config {
    /// Custom endpoint, e.g. `http://localhost:9000` for MinIO. When `None`,
    /// the standard AWS endpoint for `region` is used.
    pub endpoint: Option<String>,
    /// Region used for SigV4 signing. Defaults to `us-east-1`.
    pub region: String,
    /// Bucket name (required).
    pub bucket: String,
    /// Key prefix inside the bucket under which all Reef objects live.
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Use path-style addressing (`endpoint/bucket/key`). Required for MinIO;
    /// defaults to `true` whenever a custom endpoint is configured.
    pub force_path_style: bool,
    /// Maximum retry attempts for retryable failures (default 4).
    pub max_retries: u32,
    /// Per-request timeout (default 30s).
    pub request_timeout: Duration,
    /// Connect timeout (default 5s).
    pub connect_timeout: Duration,
}

impl S3Config {
    /// Create a config for `bucket` with library defaults and empty credentials.
    pub fn new(bucket: impl Into<String>) -> Self {
        S3Config {
            endpoint: None,
            region: "us-east-1".to_string(),
            bucket: bucket.into(),
            prefix: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: None,
            force_path_style: false,
            max_retries: 4,
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// Build a config entirely from the environment. Requires `REEF_S3_BUCKET`.
    pub fn from_env() -> StoreResult<Self> {
        let bucket = env_any(&["REEF_S3_BUCKET"]).ok_or_else(|| {
            StoreError::Config("REEF_S3_BUCKET is not set".into())
        })?;
        let prefix = env_any(&["REEF_S3_PREFIX"]).unwrap_or_default();
        Self::from_env_for(&bucket, &prefix)
    }

    /// Build a config for an explicit bucket/prefix, taking endpoint, region,
    /// and credentials from the environment.
    pub fn from_env_for(bucket: &str, prefix: &str) -> StoreResult<Self> {
        let endpoint = env_any(&["REEF_S3_ENDPOINT", "AWS_ENDPOINT_URL"]);
        let region = env_any(&["REEF_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"])
            .unwrap_or_else(|| "us-east-1".to_string());
        let access_key_id = env_any(&["REEF_S3_ACCESS_KEY", "AWS_ACCESS_KEY_ID"])
            .ok_or_else(|| StoreError::Config("AWS_ACCESS_KEY_ID is not set".into()))?;
        let secret_access_key = env_any(&["REEF_S3_SECRET_KEY", "AWS_SECRET_ACCESS_KEY"])
            .ok_or_else(|| StoreError::Config("AWS_SECRET_ACCESS_KEY is not set".into()))?;
        let session_token = env_any(&["AWS_SESSION_TOKEN"]);
        let force_path_style = match env_any(&["REEF_S3_FORCE_PATH_STYLE"]) {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
            None => endpoint.is_some(), // custom endpoints (MinIO etc.) default to path style
        };
        let mut cfg = S3Config::new(bucket);
        cfg.endpoint = endpoint;
        cfg.region = region;
        cfg.prefix = prefix.trim_matches('/').to_string();
        cfg.access_key_id = access_key_id;
        cfg.secret_access_key = secret_access_key;
        cfg.session_token = session_token;
        cfg.force_path_style = force_path_style;
        Ok(cfg)
    }
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key_id", &mask(&self.access_key_id))
            .field("secret_access_key", &"<redacted>")
            .field("session_token", &self.session_token.as_ref().map(|_| "<redacted>"))
            .field("force_path_style", &self.force_path_style)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

fn mask(s: &str) -> String {
    if s.len() <= 4 {
        "****".to_string()
    } else {
        format!("{}****", &s[..4])
    }
}

fn env_any(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|n| std::env::var(n).ok())
        .filter(|v| !v.is_empty())
}

/// S3-compatible object store.
#[derive(Debug)]
pub struct S3Store {
    cfg: S3Config,
    client: reqwest::Client,
    /// `scheme://authority` used for request URLs.
    base: String,
    /// Value of the signed `Host` header.
    host: String,
    /// Normalized key prefix (no leading/trailing slashes).
    key_prefix: String,
}

impl S3Store {
    pub fn new(cfg: S3Config) -> StoreResult<Self> {
        if cfg.bucket.is_empty() {
            return Err(StoreError::Config("s3 bucket must not be empty".into()));
        }
        if cfg.access_key_id.is_empty() || cfg.secret_access_key.is_empty() {
            return Err(StoreError::Config("s3 credentials are not configured".into()));
        }
        let endpoint = cfg
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://s3.{}.amazonaws.com", cfg.region));
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let (scheme, authority) = endpoint
            .split_once("://")
            .ok_or_else(|| StoreError::Config(format!("endpoint must include a scheme: {endpoint}")))?;
        let (base, host) = if cfg.force_path_style {
            (endpoint.clone(), authority.to_string())
        } else {
            (
                format!("{scheme}://{}.{}", cfg.bucket, authority),
                format!("{}.{}", cfg.bucket, authority),
            )
        };
        let client = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .connect_timeout(cfg.connect_timeout)
            .build()
            .map_err(|e| StoreError::Config(format!("failed to build http client: {e}")))?;
        let key_prefix = cfg.prefix.trim_matches('/').to_string();
        Ok(S3Store { cfg, client, base, host, key_prefix })
    }

    fn full_key(&self, key: &str) -> String {
        match (self.key_prefix.is_empty(), key.is_empty()) {
            (true, _) => key.to_string(),
            (false, true) => format!("{}/", self.key_prefix),
            (false, false) => format!("{}/{}", self.key_prefix, key),
        }
    }

    /// Returns `(canonical_uri, url_base)` for a request targeting `key`
    /// (`None` targets the bucket itself, e.g. for listings).
    fn target(&self, key: Option<&str>) -> (String, String) {
        match key {
            Some(k) => {
                let enc = uri_encode(&self.full_key(k), false);
                if self.cfg.force_path_style {
                    (format!("/{}/{}", self.cfg.bucket, enc), self.base.clone())
                } else {
                    (format!("/{enc}"), self.base.clone())
                }
            }
            None => {
                if self.cfg.force_path_style {
                    (format!("/{}", self.cfg.bucket), self.base.clone())
                } else {
                    ("/".to_string(), self.base.clone())
                }
            }
        }
    }

    fn signing_key(&self, date: &str) -> Vec<u8> {
        let k = hmac_sha256(format!("AWS4{}", self.cfg.secret_access_key).as_bytes(), date.as_bytes());
        let k = hmac_sha256(&k, self.cfg.region.as_bytes());
        let k = hmac_sha256(&k, b"s3");
        hmac_sha256(&k, b"aws4_request")
    }

    /// Build, sign, and send a single request (no retries).
    async fn send_once(
        &self,
        method: &Method,
        canonical_uri: &str,
        url_base: &str,
        query: &[(String, String)],
        extra_headers: &[(String, String)],
        body: &Bytes,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let payload_hash = if body.is_empty() {
            EMPTY_SHA256.to_string()
        } else {
            sha256_hex(body)
        };
        let (date, datetime) = amz_dates(SystemTime::now());

        // Assemble the signed header set (lowercase names, sorted).
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), self.host.clone()),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), datetime.clone()),
        ];
        if let Some(token) = &self.cfg.session_token {
            headers.push(("x-amz-security-token".into(), token.clone()));
        }
        for (name, value) in extra_headers {
            headers.push((name.to_ascii_lowercase(), value.clone()));
        }
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical_headers: String = headers
            .iter()
            .map(|(n, v)| format!("{n}:{}\n", v.trim()))
            .collect();
        let signed_headers: String = headers
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(";");

        // Canonical query string: encoded pairs in byte order.
        let mut encoded_query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
            .collect();
        encoded_query.sort();
        let canonical_query: String = encoded_query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.cfg.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let signature = hex_encode(&hmac_sha256(&self.signing_key(&date), string_to_sign.as_bytes()));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.cfg.access_key_id
        );

        let mut header_map = HeaderMap::new();
        for (name, value) in &headers {
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                header_map.insert(n, v);
            }
        }
        if let Ok(v) = HeaderValue::from_str(&authorization) {
            header_map.insert(reqwest::header::AUTHORIZATION, v);
        }

        let url = if canonical_query.is_empty() {
            format!("{url_base}{canonical_uri}")
        } else {
            format!("{url_base}{canonical_uri}?{canonical_query}")
        };

        self.client
            .request(method.clone(), url)
            .headers(header_map)
            .body(body.clone())
            .send()
            .await
    }

    /// Send with retries on connect errors, timeouts, 5xx, and 429.
    async fn execute(
        &self,
        method: Method,
        key: Option<&str>,
        query: Vec<(String, String)>,
        extra_headers: Vec<(String, String)>,
        body: Bytes,
    ) -> StoreResult<reqwest::Response> {
        let (canonical_uri, url_base) = self.target(key);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let result = self
                .send_once(&method, &canonical_uri, &url_base, &query, &extra_headers, &body)
                .await;
            let retryable = match &result {
                Ok(resp) => {
                    resp.status().is_server_error() || resp.status() == StatusCode::TOO_MANY_REQUESTS
                }
                Err(err) => err.is_connect() || err.is_timeout(),
            };
            if retryable && attempt <= self.cfg.max_retries {
                let delay = backoff_delay(attempt);
                tracing::warn!(
                    target = "reef_store::s3",
                    method = %method,
                    uri = %canonical_uri,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "retrying s3 request"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            return result.map_err(|e| StoreError::Backend(format!("s3 request failed: {e}")));
        }
    }

    /// Convert an error response into a [`StoreError`], consuming the body.
    async fn fail(key: &str, resp: reqwest::Response) -> StoreError {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(512).collect();
        match status {
            StatusCode::NOT_FOUND => StoreError::NotFound(key.to_string()),
            StatusCode::PRECONDITION_FAILED | StatusCode::CONFLICT => {
                StoreError::AlreadyExists(key.to_string())
            }
            _ => StoreError::Backend(format!("s3 returned {status} for {key}: {snippet}")),
        }
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put(&self, key: &str, data: Bytes) -> StoreResult<()> {
        let resp = self
            .execute(Method::PUT, Some(key), vec![], vec![], data)
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Self::fail(key, resp).await)
        }
    }

    async fn put_if_absent(&self, key: &str, data: Bytes) -> StoreResult<()> {
        let headers = vec![("if-none-match".to_string(), "*".to_string())];
        let resp = self
            .execute(Method::PUT, Some(key), vec![], headers, data)
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Self::fail(key, resp).await)
        }
    }

    async fn get(&self, key: &str) -> StoreResult<Bytes> {
        let resp = self
            .execute(Method::GET, Some(key), vec![], vec![], Bytes::new())
            .await?;
        if resp.status().is_success() {
            resp.bytes()
                .await
                .map_err(|e| StoreError::Backend(format!("failed to read body of {key}: {e}")))
        } else {
            Err(Self::fail(key, resp).await)
        }
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let end = offset + len - 1;
        let headers = vec![("range".to_string(), format!("bytes={offset}-{end}"))];
        let resp = self
            .execute(Method::GET, Some(key), vec![], headers, Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT | StatusCode::OK => resp
                .bytes()
                .await
                .map_err(|e| StoreError::Backend(format!("failed to read range of {key}: {e}"))),
            StatusCode::RANGE_NOT_SATISFIABLE => Err(StoreError::Backend(format!(
                "range {offset}..={end} not satisfiable for {key}"
            ))),
            _ => Err(Self::fail(key, resp).await),
        }
    }

    async fn head(&self, key: &str) -> StoreResult<ObjectMeta> {
        let resp = self
            .execute(Method::HEAD, Some(key), vec![], vec![], Bytes::new())
            .await?;
        if !resp.status().is_success() {
            // HEAD responses have no body; map by status code directly.
            return Err(match resp.status() {
                StatusCode::NOT_FOUND => StoreError::NotFound(key.to_string()),
                s => StoreError::Backend(format!("s3 returned {s} for HEAD {key}")),
            });
        }
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_matches('"').to_string());
        let last_modified_ms = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_http_date_ms);
        Ok(ObjectMeta { key: key.to_string(), size, etag, last_modified_ms })
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        let resp = self
            .execute(Method::DELETE, Some(key), vec![], vec![], Bytes::new())
            .await?;
        // S3 DELETE is idempotent: 204 for deleted, 404 also acceptable.
        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::fail(key, resp).await)
        }
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>> {
        let full_prefix = self.full_key(prefix);
        let strip = if self.key_prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.key_prefix)
        };
        let mut out: Vec<ObjectMeta> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), full_prefix.clone()),
                ("max-keys".into(), "1000".into()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".into(), t.clone()));
            }
            let resp = self
                .execute(Method::GET, None, query, vec![], Bytes::new())
                .await?;
            if !resp.status().is_success() {
                return Err(Self::fail(prefix, resp).await);
            }
            let body = resp
                .bytes()
                .await
                .map_err(|e| StoreError::Backend(format!("failed to read list response: {e}")))?;
            let page = parse_list(&body)?;
            for mut meta in page.objects {
                if !strip.is_empty() {
                    if let Some(rest) = meta.key.strip_prefix(&strip) {
                        meta.key = rest.to_string();
                    }
                }
                out.push(meta);
            }
            match page.next_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// SigV4 helpers
// ---------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

fn hex_encode(data: &[u8]) -> String {
    hex::encode(data)
}

/// AWS-style URI encoding: unreserved characters pass through, `/` passes
/// through unless `encode_slash`, everything else is `%XX` uppercase.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 100u64.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(5));
    let capped = base_ms.min(2_000);
    let jitter = rand::thread_rng().gen_range(0..50);
    Duration::from_millis(capped + jitter)
}

// ---------------------------------------------------------------------------
// Date handling (no chrono dependency; Howard Hinnant's civil algorithms)
// ---------------------------------------------------------------------------

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Returns `(YYYYMMDD, YYYYMMDDTHHMMSSZ)` for SigV4.
fn amz_dates(now: SystemTime) -> (String, String) {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let date = format!("{y:04}{m:02}{d:02}");
    let datetime = format!("{date}T{h:02}{mi:02}{s:02}Z");
    (date, datetime)
}

/// Parse an ISO-8601 timestamp (`2024-01-02T03:04:05.678Z`) to epoch millis.
fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let y = num(0..4)?;
    let m = num(5..7)? as u32;
    let d = num(8..10)? as u32;
    let h = num(11..13)?;
    let mi = num(14..16)?;
    let sec = num(17..19)?;
    let mut ms: i64 = 0;
    if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !frac.is_empty() {
            let mut v = frac.parse::<i64>().ok()?;
            match frac.len() {
                1 => v *= 100,
                2 => v *= 10,
                3 => {}
                n => v /= 10i64.pow((n - 3) as u32),
            }
            ms = v;
        }
    }
    Some((days_from_civil(y, m, d) * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000 + ms)
}

/// Parse an RFC-1123 HTTP date (`Tue, 02 Jan 2024 03:04:05 GMT`) to epoch millis.
fn parse_http_date_ms(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let d: u32 = parts[1].parse().ok()?;
    let m = match parts[2] {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let y: i64 = parts[3].parse().ok()?;
    let mut hms = parts[4].split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.parse().ok()?;
    Some((days_from_civil(y, m, d) * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000)
}

// ---------------------------------------------------------------------------
// ListObjectsV2 response parsing
// ---------------------------------------------------------------------------

struct ListPage {
    objects: Vec<ObjectMeta>,
    next_token: Option<String>,
}

fn parse_list(xml: &[u8]) -> StoreResult<ListPage> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut in_contents = false;
    let mut text = String::new();
    let mut cur_key = String::new();
    let mut cur_size: u64 = 0;
    let mut cur_etag: Option<String> = None;
    let mut cur_modified: Option<i64> = None;
    let mut objects: Vec<ObjectMeta> = Vec::new();
    let mut next_token: Option<String> = None;
    let mut truncated = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "Contents" {
                    in_contents = true;
                    cur_key.clear();
                    cur_size = 0;
                    cur_etag = None;
                    cur_modified = None;
                }
                text.clear();
            }
            Ok(Event::Text(t)) => {
                let unescaped = t
                    .unescape()
                    .map_err(|e| StoreError::Backend(format!("bad XML text in list response: {e}")))?;
                text.push_str(&unescaped);
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "Contents" => {
                        in_contents = false;
                        objects.push(ObjectMeta {
                            key: cur_key.clone(),
                            size: cur_size,
                            etag: cur_etag.clone(),
                            last_modified_ms: cur_modified,
                        });
                    }
                    "Key" if in_contents => cur_key = text.clone(),
                    "Size" if in_contents => cur_size = text.parse().unwrap_or(0),
                    "ETag" if in_contents => cur_etag = Some(text.trim_matches('"').to_string()),
                    "LastModified" if in_contents => cur_modified = parse_iso8601_ms(&text),
                    "NextContinuationToken" => next_token = Some(text.clone()),
                    "IsTruncated" => truncated = text == "true",
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(StoreError::Backend(format!(
                    "failed to parse S3 list response: {e}"
                )))
            }
        }
    }
    if !truncated {
        next_token = None;
    }
    Ok(ListPage { objects, next_token })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_matches_aws_rules() {
        assert_eq!(uri_encode("abc-XYZ_0.9~", true), "abc-XYZ_0.9~");
        assert_eq!(uri_encode("a b", true), "a%20b");
        assert_eq!(uri_encode("a/b", false), "a/b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("ns/wal/0001.wal", false), "ns/wal/0001.wal");
        assert_eq!(uri_encode("k=v&x", true), "k%3Dv%26x");
    }

    #[test]
    fn civil_date_roundtrip() {
        for days in [-1000i64, 0, 1, 19_723, 20_000, 365 * 100] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "roundtrip for {days}");
        }
        // Known anchor: 1970-01-01 is day 0; 2024-01-01 is day 19723.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn amz_date_formatting() {
        // 2024-01-02T03:04:05Z = 1704164645
        let t = UNIX_EPOCH + Duration::from_secs(1_704_164_645);
        let (date, datetime) = amz_dates(t);
        assert_eq!(date, "20240102");
        assert_eq!(datetime, "20240102T030405Z");
    }

    #[test]
    fn iso8601_parsing() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2024-01-02T03:04:05.678Z"),
            Some(1_704_164_645_678)
        );
        assert_eq!(parse_iso8601_ms("2024-01-02T03:04:05Z"), Some(1_704_164_645_000));
        assert_eq!(parse_iso8601_ms("garbage"), None);
    }

    #[test]
    fn http_date_parsing() {
        assert_eq!(
            parse_http_date_ms("Tue, 02 Jan 2024 03:04:05 GMT"),
            Some(1_704_164_645_000)
        );
        assert_eq!(parse_http_date_ms("not a date"), None);
    }

    #[test]
    fn list_response_parsing() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>reef-test</Name>
  <Prefix>tenant/ns1/</Prefix>
  <KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>token-abc</NextContinuationToken>
  <Contents>
    <Key>tenant/ns1/wal/00000001.wal</Key>
    <LastModified>2024-01-02T03:04:05.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag>
    <Size>1234</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>tenant/ns1/segments/a&amp;b.seg</Key>
    <LastModified>2024-01-02T03:04:06.000Z</LastModified>
    <ETag>"abcd"</ETag>
    <Size>5678</Size>
  </Contents>
</ListBucketResult>"#;
        let page = parse_list(xml).unwrap();
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].key, "tenant/ns1/wal/00000001.wal");
        assert_eq!(page.objects[0].size, 1234);
        assert_eq!(
            page.objects[0].etag.as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(page.objects[0].last_modified_ms, Some(1_704_164_645_000));
        assert_eq!(page.objects[1].key, "tenant/ns1/segments/a&b.seg");
        assert_eq!(page.next_token.as_deref(), Some("token-abc"));
    }

    #[test]
    fn list_response_not_truncated_clears_token() {
        let xml = br#"<ListBucketResult><IsTruncated>false</IsTruncated>
            <NextContinuationToken>stale</NextContinuationToken>
            <Contents><Key>k</Key><Size>1</Size></Contents></ListBucketResult>"#;
        let page = parse_list(xml).unwrap();
        assert_eq!(page.objects.len(), 1);
        assert!(page.next_token.is_none());
    }

    #[test]
    fn config_debug_redacts_secrets() {
        let mut cfg = S3Config::new("bucket");
        cfg.access_key_id = "AKIAEXAMPLEKEY".into();
        cfg.secret_access_key = "supersecretvalue".into();
        cfg.session_token = Some("tokenvalue".into());
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("supersecretvalue"));
        assert!(!dbg.contains("tokenvalue"));
        assert!(dbg.contains("AKIA****"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn signing_key_is_deterministic_and_date_scoped() {
        let mut cfg = S3Config::new("b");
        cfg.access_key_id = "AKID".into();
        cfg.secret_access_key = "SECRET".into();
        cfg.endpoint = Some("http://localhost:9000".into());
        cfg.force_path_style = true;
        let store = S3Store::new(cfg).unwrap();
        let k1 = store.signing_key("20240101");
        let k2 = store.signing_key("20240101");
        let k3 = store.signing_key("20240102");
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
        assert_eq!(k1.len(), 32);
    }

    #[test]
    fn target_paths() {
        let mut cfg = S3Config::new("bkt");
        cfg.access_key_id = "a".into();
        cfg.secret_access_key = "s".into();
        cfg.endpoint = Some("http://localhost:9000".into());
        cfg.force_path_style = true;
        cfg.prefix = "tenant".into();
        let store = S3Store::new(cfg).unwrap();
        let (uri, base) = store.target(Some("ns/wal/1.wal"));
        assert_eq!(uri, "/bkt/tenant/ns/wal/1.wal");
        assert_eq!(base, "http://localhost:9000");
        let (uri, _) = store.target(None);
        assert_eq!(uri, "/bkt");
    }
}
