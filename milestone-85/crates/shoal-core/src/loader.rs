//! Segment loading abstraction.
//!
//! The engine never reads segment objects directly: it goes through a
//! [`SegmentLoader`], which lets deployments plug in the tiered cache from
//! the `shoal-cache` crate (memory LRU -> local disk -> object storage)
//! without the core depending on it. [`DirectLoader`] reads straight from
//! object storage and is used in tests and minimal deployments.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::segment::SegmentData;
use crate::storage::ObjectStorage;

#[async_trait]
pub trait SegmentLoader: Send + Sync {
    /// Load (and decode) the segment stored at `key`.
    async fn load(&self, key: &str) -> Result<Arc<SegmentData>>;

    /// Drop any cached copies of `key` (called when a segment is physically
    /// deleted after its refcount reaches zero).
    fn invalidate(&self, _key: &str) {}
}

pub struct DirectLoader {
    storage: Arc<ObjectStorage>,
}

impl DirectLoader {
    pub fn new(storage: Arc<ObjectStorage>) -> Self {
        DirectLoader { storage }
    }
}

#[async_trait]
impl SegmentLoader for DirectLoader {
    async fn load(&self, key: &str) -> Result<Arc<SegmentData>> {
        let bytes = self.storage.get(key).await?;
        Ok(Arc::new(SegmentData::decode(&bytes)?))
    }
}
