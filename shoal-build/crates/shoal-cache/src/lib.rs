//! # shoal-cache
//!
//! Two-tier cache hierarchy for Shoal compute nodes.
//!
//! Shoal keeps the durable source of truth in object storage. Query latency is
//! dominated by object-storage round trips, so compute nodes keep two caches:
//!
//! 1. **Memory tier** ([`memory::MemoryCache`]): a weight-bounded LRU holding
//!    hot, decoded-or-raw values (manifests, index headers, small segments).
//! 2. **Disk tier** ([`disk::DiskCache`]): a byte-bounded LRU of raw object
//!    bodies on local SSD. Entries survive process restarts: the cache directory
//!    is rescanned on startup and incomplete writes are discarded.
//!
//! [`layer::CacheLayer`] composes the two tiers in front of an arbitrary
//! origin-fetch closure (normally an object-storage GET) and reports which tier
//! served each request so the server can export cold/warm hit-rate metrics.
//!
//! ## Pinning
//!
//! Both tiers support *prefix pinning*. Cache keys are object-storage keys, so a
//! namespace can be kept warm by pinning its key prefix
//! (e.g. `orgs/acme/projects/web/ns/docs/`). Pinned entries are never evicted by
//! the LRU policy (they can still be explicitly invalidated). Disk-tier pins are
//! persisted in `pins.json` inside the cache directory and survive restarts.
//!
//! ## Crash safety
//!
//! Disk entries are written as `data.tmp -> rename(data) -> write(meta)`. On
//! startup, any `.tmp` file, any meta without a data file, any data file without
//! meta, and any size mismatch between meta and data is treated as an incomplete
//! or corrupt write and deleted. The cache is only a cache: losing entries is
//! always safe because the origin (object storage) is authoritative.

pub mod disk;
pub mod layer;
pub mod memory;
pub mod stats;

pub use disk::{DiskCache, DiskCacheConfig};
pub use layer::{CacheLayer, CacheLayerConfig, CacheTier};
pub use memory::MemoryCache;
pub use stats::{CounterSet, CounterSnapshot, DiskTierSnapshot, LayerSnapshot, TierSnapshot, TierStats};
