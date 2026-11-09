//! Optional per-key token-bucket rate limits and quota configuration.
//!
//! Disabled by default. When enabled, each API key gets two buckets: one for
//! request count and one for write bytes. Buckets refill continuously; a
//! request that cannot afford its cost is rejected with a computed
//! `Retry-After`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub requests_per_second: f64,
    /// Maximum burst of requests above the steady rate.
    pub burst: f64,
    /// Write throughput limit per key; 0 disables the byte limit.
    pub write_bytes_per_second: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            requests_per_second: 100.0,
            burst: 200.0,
            write_bytes_per_second: 0.0,
        }
    }
}

/// Hard quotas enforced by the namespace engine (not time-based).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    pub max_namespaces_per_project: Option<u64>,
    pub max_batch_documents: Option<usize>,
    pub max_document_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimited {
    pub retry_after_secs: f64,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate limited; retry after {:.2}s", self.retry_after_secs)
    }
}

impl std::error::Error for RateLimited {}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(burst: f64, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    fn take(&mut self, cost: f64, rate: f64, burst: f64, now: Instant) -> Result<(), RateLimited> {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.last = now;
        if self.tokens >= cost {
            self.tokens -= cost;
            Ok(())
        } else {
            Err(RateLimited {
                retry_after_secs: (cost - self.tokens) / rate,
            })
        }
    }
}

const MAX_TRACKED_KEYS: usize = 8192;
const IDLE_EVICT: Duration = Duration::from_secs(600);

pub struct RateLimiter {
    cfg: RateLimitConfig,
    requests: Mutex<HashMap<String, Bucket>>,
    writes: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            cfg,
            requests: Mutex::new(HashMap::new()),
            writes: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Charge one request against `key_id`'s request bucket.
    pub fn check_request(&self, key_id: &str) -> Result<(), RateLimited> {
        self.check_request_at(key_id, Instant::now())
    }

    fn check_request_at(&self, key_id: &str, now: Instant) -> Result<(), RateLimited> {
        if !self.cfg.enabled || self.cfg.requests_per_second <= 0.0 {
            return Ok(());
        }
        let rate = self.cfg.requests_per_second;
        let burst = self.cfg.burst.max(rate);
        let mut map = self.requests.lock();
        Self::gc(&mut map, now);
        map.entry(key_id.to_string())
            .or_insert_with(|| Bucket::new(burst, now))
            .take(1.0, rate, burst, now)
    }

    /// Charge `bytes` against `key_id`'s write-throughput bucket.
    pub fn check_write(&self, key_id: &str, bytes: u64) -> Result<(), RateLimited> {
        self.check_write_at(key_id, bytes, Instant::now())
    }

    fn check_write_at(&self, key_id: &str, bytes: u64, now: Instant) -> Result<(), RateLimited> {
        if !self.cfg.enabled || self.cfg.write_bytes_per_second <= 0.0 {
            return Ok(());
        }
        let rate = self.cfg.write_bytes_per_second;
        let burst = rate * 2.0;
        let mut map = self.writes.lock();
        Self::gc(&mut map, now);
        map.entry(key_id.to_string())
            .or_insert_with(|| Bucket::new(burst, now))
            .take(bytes as f64, rate, burst, now)
    }

    fn gc(map: &mut HashMap<String, Bucket>, now: Instant) {
        if map.len() > MAX_TRACKED_KEYS {
            map.retain(|_, b| now.saturating_duration_since(b.last) < IDLE_EVICT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(rps: f64, burst: f64, wbps: f64) -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            requests_per_second: rps,
            burst,
            write_bytes_per_second: wbps,
        }
    }

    #[test]
    fn disabled_always_allows() {
        let rl = RateLimiter::new(RateLimitConfig::default());
        for _ in 0..10_000 {
            assert!(rl.check_request("k").is_ok());
        }
    }

    #[test]
    fn burst_then_limited_then_refill() {
        let rl = RateLimiter::new(cfg(10.0, 5.0, 0.0));
        let t0 = Instant::now();
        for _ in 0..10 {
            // burst.max(rate) = 10 tokens available initially
            assert!(rl.check_request_at("k", t0).is_ok());
        }
        let denied = rl.check_request_at("k", t0).unwrap_err();
        assert!(denied.retry_after_secs > 0.0);

        // After one second, 10 more tokens are available.
        let t1 = t0 + Duration::from_secs(1);
        for _ in 0..10 {
            assert!(rl.check_request_at("k", t1).is_ok());
        }
        assert!(rl.check_request_at("k", t1).is_err());
    }

    #[test]
    fn keys_are_isolated() {
        let rl = RateLimiter::new(cfg(1.0, 1.0, 0.0));
        let t0 = Instant::now();
        assert!(rl.check_request_at("a", t0).is_ok());
        assert!(rl.check_request_at("a", t0).is_err());
        assert!(rl.check_request_at("b", t0).is_ok());
    }

    #[test]
    fn write_bytes_limit() {
        let rl = RateLimiter::new(cfg(1000.0, 1000.0, 100.0));
        let t0 = Instant::now();
        // burst = 2x rate = 200 bytes
        assert!(rl.check_write_at("k", 150, t0).is_ok());
        assert!(rl.check_write_at("k", 100, t0).is_err());
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.check_write_at("k", 100, t1).is_ok());
    }

    #[test]
    fn zero_write_rate_is_unlimited() {
        let rl = RateLimiter::new(cfg(10.0, 10.0, 0.0));
        assert!(rl.check_write("k", u64::MAX / 2).is_ok());
    }
}
