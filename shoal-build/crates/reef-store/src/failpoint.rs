//! Failure-injection wrapper for crash-recovery testing.
//!
//! `FailpointStore` wraps any [`ObjectStore`] and can be armed to fail write
//! operations at a precise point — e.g. "the 3rd put whose key contains
//! `segments/`". Two injection modes model the two interesting crash shapes:
//!
//! - [`InjectMode::ErrBeforeWrite`]: the operation fails and the object was
//!   **not** written (process died before the upload completed).
//! - [`InjectMode::ErrAfterWrite`]: the object **was** written but the caller
//!   sees an error (upload succeeded, process died before observing the
//!   response — the classic ambiguous-outcome case).
//!
//! Once a failpoint triggers, the store enters a *halted* state where every
//! subsequent operation fails, simulating the rest of the crashed process's
//! lifetime. Tests then call [`FailpointStore::resume`] (the "restart") and
//! re-open the engine over the same store to exercise recovery.

use crate::{ObjectMeta, ObjectStore, StoreError, StoreResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Where the injected failure lands relative to the underlying write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectMode {
    /// Fail without performing the write.
    ErrBeforeWrite,
    /// Perform the write, then fail.
    ErrAfterWrite,
}

#[derive(Debug)]
struct FailPlan {
    /// Fail the Nth matching write (1-based). `None` = disarmed.
    fail_on_matching_write: Option<u64>,
    /// Only writes whose key contains this substring count. `None` = all.
    match_substring: Option<String>,
    mode: InjectMode,
    halted: bool,
}

/// Failure-injecting object store wrapper.
#[derive(Debug)]
pub struct FailpointStore {
    inner: Arc<dyn ObjectStore>,
    plan: Mutex<FailPlan>,
    write_count: AtomicU64,
    matched_count: AtomicU64,
}

impl FailpointStore {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        FailpointStore {
            inner,
            plan: Mutex::new(FailPlan {
                fail_on_matching_write: None,
                match_substring: None,
                mode: InjectMode::ErrBeforeWrite,
                halted: false,
            }),
            write_count: AtomicU64::new(0),
            matched_count: AtomicU64::new(0),
        }
    }

    /// Arm the failpoint: fail the `nth` (1-based) write whose key contains
    /// `substring` (or any write if `substring` is `None`), with the given
    /// mode. Resets counters.
    pub fn arm(&self, nth: u64, substring: Option<&str>, mode: InjectMode) {
        assert!(nth >= 1, "nth is 1-based");
        let mut plan = self.plan.lock().unwrap();
        plan.fail_on_matching_write = Some(nth);
        plan.match_substring = substring.map(|s| s.to_string());
        plan.mode = mode;
        plan.halted = false;
        self.matched_count.store(0, Ordering::SeqCst);
    }

    /// Disarm injection and clear the halted state — simulates restarting
    /// the process over the same durable store.
    pub fn resume(&self) {
        let mut plan = self.plan.lock().unwrap();
        plan.fail_on_matching_write = None;
        plan.match_substring = None;
        plan.halted = false;
        self.matched_count.store(0, Ordering::SeqCst);
    }

    /// Total writes attempted through this wrapper (test assertions).
    pub fn write_count(&self) -> u64 {
        self.write_count.load(Ordering::SeqCst)
    }

    pub fn is_halted(&self) -> bool {
        self.plan.lock().unwrap().halted
    }

    /// Decide whether this write should be failed, and in which mode.
    /// Returns `Err` immediately if the store is already halted.
    fn check_write(&self, path: &str) -> StoreResult<Option<InjectMode>> {
        self.write_count.fetch_add(1, Ordering::SeqCst);
        let mut plan = self.plan.lock().unwrap();
        if plan.halted {
            return Err(StoreError::Injected(format!(
                "store halted (simulated crash); rejected write to {path}"
            )));
        }
        let Some(nth) = plan.fail_on_matching_write else {
            return Ok(None);
        };
        let matches = plan
            .match_substring
            .as_deref()
            .map(|s| path.contains(s))
            .unwrap_or(true);
        if !matches {
            return Ok(None);
        }
        let count = self.matched_count.fetch_add(1, Ordering::SeqCst) + 1;
        if count == nth {
            plan.halted = true;
            Ok(Some(plan.mode))
        } else {
            Ok(None)
        }
    }

    fn check_read(&self, path: &str) -> StoreResult<()> {
        let plan = self.plan.lock().unwrap();
        if plan.halted {
            return Err(StoreError::Injected(format!(
                "store halted (simulated crash); rejected read of {path}"
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for FailpointStore {
    async fn put(&self, path: &str, data: Bytes) -> StoreResult<()> {
        match self.check_write(path)? {
            None => self.inner.put(path, data).await,
            Some(InjectMode::ErrBeforeWrite) => Err(StoreError::Injected(format!(
                "injected failure before put of {path}"
            ))),
            Some(InjectMode::ErrAfterWrite) => {
                self.inner.put(path, data).await?;
                Err(StoreError::Injected(format!(
                    "injected failure after put of {path}"
                )))
            }
        }
    }

    async fn put_if_absent(&self, path: &str, data: Bytes) -> StoreResult<()> {
        match self.check_write(path)? {
            None => self.inner.put_if_absent(path, data).await,
            Some(InjectMode::ErrBeforeWrite) => Err(StoreError::Injected(format!(
                "injected failure before put_if_absent of {path}"
            ))),
            Some(InjectMode::ErrAfterWrite) => {
                self.inner.put_if_absent(path, data).await?;
                Err(StoreError::Injected(format!(
                    "injected failure after put_if_absent of {path}"
                )))
            }
        }
    }

    async fn get(&self, path: &str) -> StoreResult<Bytes> {
        self.check_read(path)?;
        self.inner.get(path).await
    }

    async fn get_range(&self, path: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        self.check_read(path)?;
        self.inner.get_range(path, offset, len).await
    }

    async fn head(&self, path: &str) -> StoreResult<ObjectMeta> {
        self.check_read(path)?;
        self.inner.head(path).await
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>> {
        self.check_read(prefix)?;
        self.inner.list(prefix).await
    }

    async fn delete(&self, path: &str) -> StoreResult<()> {
        match self.check_write(path)? {
            None => self.inner.delete(path).await,
            Some(InjectMode::ErrBeforeWrite) => Err(StoreError::Injected(format!(
                "injected failure before delete of {path}"
            ))),
            Some(InjectMode::ErrAfterWrite) => {
                self.inner.delete(path).await?;
                Err(StoreError::Injected(format!(
                    "injected failure after delete of {path}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;

    fn wrapped() -> (MemoryStore, FailpointStore) {
        let mem = MemoryStore::new();
        let fp = FailpointStore::new(Arc::new(mem.clone()));
        (mem, fp)
    }

    #[tokio::test]
    async fn passthrough_when_disarmed() {
        let (_mem, fp) = wrapped();
        fp.put("a", Bytes::from_static(b"x")).await.unwrap();
        assert_eq!(fp.get("a").await.unwrap(), Bytes::from_static(b"x"));
        assert_eq!(fp.write_count(), 1);
    }

    #[tokio::test]
    async fn err_before_write_leaves_no_object() {
        let (mem, fp) = wrapped();
        fp.arm(1, None, InjectMode::ErrBeforeWrite);
        let err = fp.put("a", Bytes::from_static(b"x")).await.unwrap_err();
        assert!(matches!(err, StoreError::Injected(_)));
        assert!(!mem.exists("a").await.unwrap());
        assert!(fp.is_halted());
    }

    #[tokio::test]
    async fn err_after_write_leaves_object() {
        let (mem, fp) = wrapped();
        fp.arm(1, None, InjectMode::ErrAfterWrite);
        let err = fp.put("a", Bytes::from_static(b"x")).await.unwrap_err();
        assert!(matches!(err, StoreError::Injected(_)));
        assert!(mem.exists("a").await.unwrap(), "torn write must persist");
    }

    #[tokio::test]
    async fn nth_matching_write_triggers() {
        let (mem, fp) = wrapped();
        fp.arm(2, Some("segments/"), InjectMode::ErrBeforeWrite);
        // Non-matching writes never count.
        fp.put("wal/1", Bytes::from_static(b"x")).await.unwrap();
        fp.put("wal/2", Bytes::from_static(b"x")).await.unwrap();
        // First matching write passes.
        fp.put("segments/s1", Bytes::from_static(b"x")).await.unwrap();
        // Second matching write fails.
        assert!(fp.put("segments/s2", Bytes::from_static(b"x")).await.is_err());
        assert!(!mem.exists("segments/s2").await.unwrap());
    }

    #[tokio::test]
    async fn halted_blocks_everything_until_resume() {
        let (_mem, fp) = wrapped();
        fp.arm(1, None, InjectMode::ErrBeforeWrite);
        let _ = fp.put("a", Bytes::from_static(b"x")).await;
        assert!(fp.get("a").await.is_err());
        assert!(fp.list("").await.is_err());
        assert!(fp.put("b", Bytes::from_static(b"y")).await.is_err());

        fp.resume();
        fp.put("b", Bytes::from_static(b"y")).await.unwrap();
        assert_eq!(fp.get("b").await.unwrap(), Bytes::from_static(b"y"));
    }
}
