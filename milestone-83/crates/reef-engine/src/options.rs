//! Tunable knobs for the storage engine, indexer, compaction and GC.

/// Engine configuration. All values have conservative defaults suitable for
/// production; `EngineOptions::for_tests()` returns an aggressive profile
/// that folds/compacts/GCs immediately so tests can exercise every path.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Fold the memtable into a level-0 segment as soon as it holds at least
    /// this many document entries (checked by the maintenance loop).
    pub fold_max_memtable_entries: usize,
    /// Fold the memtable as soon as its approximate size reaches this many
    /// bytes.
    pub fold_max_memtable_bytes: u64,
    /// Cadence (in maintenance ticks) for folding small, non-urgent
    /// memtables. `1` folds on every tick; `2` folds every other tick.
    pub fold_every_n_ticks: u32,
    /// Maintenance loop tick interval in milliseconds.
    pub maintenance_interval_ms: u64,
    /// Trigger compaction when at least this many level-0 segments exist.
    pub l0_compact_trigger: usize,
    /// Target (approximate) size of compacted level-1 segments. Compaction
    /// splits its output into multiple segments of roughly this size.
    pub compaction_target_segment_bytes: u64,
    /// Objects that are unreferenced by the current manifest are only
    /// garbage-collected once they are at least this old. This protects
    /// in-flight folds/compactions in other processes from having their
    /// freshly written (but not yet committed) objects deleted.
    pub gc_grace_ms: u64,
    /// Run GC every N maintenance ticks.
    pub gc_every_n_ticks: u32,
    /// Number of historical manifest versions to retain (the current version
    /// is always retained). Older versions enable point-in-time debugging
    /// and manifest rollback during repair.
    pub keep_manifests: usize,
    /// How long idempotency keys are remembered. Replays of a write with a
    /// previously seen key inside this window return the original result
    /// instead of committing a duplicate.
    pub idempotency_retention_ms: u64,
    /// Hard cap on remembered idempotency keys per namespace (oldest are
    /// evicted first).
    pub idempotency_max_entries: usize,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            fold_max_memtable_entries: 5_000,
            fold_max_memtable_bytes: 32 * 1024 * 1024,
            fold_every_n_ticks: 2,
            maintenance_interval_ms: 1_000,
            l0_compact_trigger: 8,
            compaction_target_segment_bytes: 64 * 1024 * 1024,
            gc_grace_ms: 5 * 60 * 1000,
            gc_every_n_ticks: 60,
            keep_manifests: 5,
            idempotency_retention_ms: 24 * 60 * 60 * 1000,
            idempotency_max_entries: 10_000,
        }
    }
}

impl EngineOptions {
    /// An aggressive profile for tests: tiny thresholds, zero GC grace, fast
    /// maintenance ticks.
    pub fn for_tests() -> Self {
        Self {
            fold_max_memtable_entries: 4,
            fold_max_memtable_bytes: 64 * 1024,
            fold_every_n_ticks: 1,
            maintenance_interval_ms: 25,
            l0_compact_trigger: 2,
            compaction_target_segment_bytes: 256 * 1024,
            gc_grace_ms: 0,
            gc_every_n_ticks: 2,
            keep_manifests: 3,
            idempotency_retention_ms: 60 * 60 * 1000,
            idempotency_max_entries: 1_000,
        }
    }
}
