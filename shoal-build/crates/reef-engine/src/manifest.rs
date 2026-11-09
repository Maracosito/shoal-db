//! Namespace manifest: the atomically-committed root of all durable namespace state.
//!
//! A manifest is an immutable JSON object written once under a monotonically
//! increasing generation number:
//!
//! ```text
//! namespaces/<ns>/manifests/00000000000000000001.json
//! namespaces/<ns>/manifests/00000000000000000002.json
//! namespaces/<ns>/manifests/CURRENT          (advisory hint, best-effort)
//! ```
//!
//! Commit protocol (compare-and-set via `put_if_absent`):
//!
//! 1. Reader loads the latest manifest (highest decodable generation).
//! 2. Writer builds the successor manifest at `generation + 1`.
//! 3. Writer issues `put_if_absent` on the successor key. Exactly one writer
//!    can win a given generation; losers receive [`EngineError::Conflict`] and
//!    must reload and retry. This makes manifest swaps atomic even when
//!    multiple indexers/compactors race.
//! 4. Writer updates `CURRENT` best-effort. `CURRENT` is *never* trusted as
//!    authoritative — discovery always lists the manifest prefix — it only
//!    exists as a human/tooling convenience and a fast-path hint.
//!
//! Each manifest file is self-checksummed. The on-disk encoding is two JSON
//! documents separated by a newline:
//!
//! ```text
//! {"format":"reef.manifest.v1","crc32c":<u32>,"length":<u64>}\n
//! {...manifest payload...}
//! ```
//!
//! The CRC covers the raw payload bytes, so torn or bit-rotted manifests are
//! detected on read. `load_latest` skips corrupt generations (reporting them
//! to the caller) and falls back to the newest decodable one, which is always
//! safe because every manifest is a complete, self-contained snapshot of the
//! namespace state and segments are immutable.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use reef_store::ObjectStore;

use crate::checksum::crc32c;
use crate::error::{EngineError, EngineResult};
use crate::layout;

/// Format tag embedded in every manifest header. Bump on incompatible change.
pub const MANIFEST_FORMAT: &str = "reef.manifest.v1";

/// Structural version of the manifest payload itself.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reference to a single immutable segment file.
///
/// Segments are content-immutable: once a `SegmentRef` appears in any
/// manifest, the underlying object is never rewritten in place. Branching
/// relies on this — a branch manifest may reference segments physically owned
/// by an ancestor namespace via `owner_namespace`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRef {
    /// Unique segment id (ULID-style string chosen by the writer).
    pub segment_id: String,
    /// Compaction level. Level 0 = freshly flushed from WAL; higher levels
    /// are produced by compaction merges.
    pub level: u32,
    /// Total rows in the segment, including tombstone rows.
    pub doc_count: u64,
    /// Live (non-tombstone) rows.
    pub live_doc_count: u64,
    /// Tombstone rows carried to mask older segments.
    pub tombstone_count: u64,
    /// Object size in bytes.
    pub size_bytes: u64,
    /// Lowest WAL sequence folded into this segment.
    pub min_wal_seq: u64,
    /// Highest WAL sequence folded into this segment.
    pub max_wal_seq: u64,
    /// Wall-clock creation time (informational only).
    pub created_at_ms: u64,
    /// Namespace that physically owns the object. Usually equal to the
    /// manifest's namespace; differs for copy-on-write branches.
    pub owner_namespace: String,
}

/// Link to the parent namespace a branch was forked from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParentRef {
    pub namespace: String,
    /// Parent manifest generation the branch was cut at.
    pub generation: u64,
}

/// Complete, self-contained snapshot of a namespace's durable state.
///
/// Segment order is significant: segments are listed oldest-first, and for a
/// given document id, a row in a *later* segment shadows rows in earlier
/// segments. Tombstone rows shadow (delete) earlier live rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub namespace: String,
    /// Monotonic generation; commit is CAS on the generation key.
    pub generation: u64,
    /// All WAL entries with `seq <= wal_floor` are folded into segments and
    /// no longer needed for recovery. WAL files at or below the floor are
    /// garbage-collectable.
    pub wal_floor: u64,
    /// Highest WAL sequence known committed when this manifest was written.
    /// Informational; recovery always re-scans the WAL above `wal_floor`.
    pub wal_head: u64,
    /// Oldest-first segment list (see ordering note above).
    pub segments: Vec<SegmentRef>,
    /// Set when this namespace is a copy-on-write branch.
    pub parent: Option<ParentRef>,
    pub created_at_ms: u64,
    /// Free-form writer identity (node id / process) for debugging.
    pub committed_by: String,
}

impl Manifest {
    /// The very first manifest of a fresh namespace.
    pub fn initial(namespace: impl Into<String>, committed_by: impl Into<String>) -> Self {
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            namespace: namespace.into(),
            generation: 1,
            wal_floor: 0,
            wal_head: 0,
            segments: Vec::new(),
            parent: None,
            created_at_ms: unix_ms(),
            committed_by: committed_by.into(),
        }
    }

    /// Successor template: same content, next generation, fresh timestamp.
    /// Callers mutate the result (add/remove segments, advance floors) and
    /// then commit it.
    pub fn next(&self, committed_by: impl Into<String>) -> Self {
        let mut m = self.clone();
        m.generation = self.generation + 1;
        m.created_at_ms = unix_ms();
        m.committed_by = committed_by.into();
        m
    }

    pub fn total_live_docs(&self) -> u64 {
        self.segments.iter().map(|s| s.live_doc_count).sum()
    }

    pub fn total_size_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.size_bytes).sum()
    }

    pub fn segment(&self, segment_id: &str) -> Option<&SegmentRef> {
        self.segments.iter().find(|s| s.segment_id == segment_id)
    }

    /// Structural invariant checks performed before every commit and after
    /// every load.
    pub fn validate(&self) -> EngineResult<()> {
        if self.format_version != MANIFEST_FORMAT_VERSION {
            return Err(EngineError::Corruption(format!(
                "manifest format_version {} unsupported (expected {})",
                self.format_version, MANIFEST_FORMAT_VERSION
            )));
        }
        if self.generation == 0 {
            return Err(EngineError::Corruption(
                "manifest generation must be >= 1".into(),
            ));
        }
        if self.wal_floor > self.wal_head {
            return Err(EngineError::Corruption(format!(
                "manifest wal_floor {} > wal_head {}",
                self.wal_floor, self.wal_head
            )));
        }
        let mut seen = std::collections::HashSet::with_capacity(self.segments.len());
        for s in &self.segments {
            if !seen.insert(s.segment_id.as_str()) {
                return Err(EngineError::Corruption(format!(
                    "duplicate segment id {} in manifest generation {}",
                    s.segment_id, self.generation
                )));
            }
            if s.max_wal_seq > self.wal_floor {
                return Err(EngineError::Corruption(format!(
                    "segment {} max_wal_seq {} exceeds wal_floor {}",
                    s.segment_id, s.max_wal_seq, self.wal_floor
                )));
            }
            if s.min_wal_seq > s.max_wal_seq {
                return Err(EngineError::Corruption(format!(
                    "segment {} has min_wal_seq {} > max_wal_seq {}",
                    s.segment_id, s.min_wal_seq, s.max_wal_seq
                )));
            }
            if s.live_doc_count + s.tombstone_count != s.doc_count {
                return Err(EngineError::Corruption(format!(
                    "segment {} row accounting mismatch: live {} + tombstones {} != total {}",
                    s.segment_id, s.live_doc_count, s.tombstone_count, s.doc_count
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestHeader {
    format: String,
    crc32c: u32,
    length: u64,
}

/// Result of [`ManifestStore::load_latest_detailed`].
#[derive(Debug)]
pub struct ManifestLoad {
    pub manifest: Option<Manifest>,
    /// Generations whose files existed but failed checksum/decode. The
    /// `repair` admin command quarantines these.
    pub corrupted_generations: Vec<u64>,
}

/// Reads, commits and prunes manifest files for one namespace.
pub struct ManifestStore {
    store: Arc<dyn ObjectStore>,
    namespace: String,
}

impl ManifestStore {
    pub fn new(store: Arc<dyn ObjectStore>, namespace: impl Into<String>) -> Self {
        ManifestStore {
            store,
            namespace: namespace.into(),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Serialize a manifest into the checksummed two-part on-disk encoding.
    pub fn encode(manifest: &Manifest) -> EngineResult<Bytes> {
        let payload = serde_json::to_vec(manifest).map_err(|e| {
            EngineError::Corruption(format!("manifest serialization failed: {e}"))
        })?;
        let header = ManifestHeader {
            format: MANIFEST_FORMAT.to_string(),
            crc32c: crc32c(&payload),
            length: payload.len() as u64,
        };
        let mut out = serde_json::to_vec(&header).map_err(|e| {
            EngineError::Corruption(format!("manifest header serialization failed: {e}"))
        })?;
        out.push(b'\n');
        out.extend_from_slice(&payload);
        Ok(Bytes::from(out))
    }

    /// Decode and verify a manifest file. Any structural or checksum failure
    /// maps to [`EngineError::Corruption`].
    pub fn decode(bytes: &[u8]) -> EngineResult<Manifest> {
        let nl = bytes
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| EngineError::Corruption("manifest missing header delimiter".into()))?;
        let header: ManifestHeader = serde_json::from_slice(&bytes[..nl])
            .map_err(|e| EngineError::Corruption(format!("manifest header unreadable: {e}")))?;
        if header.format != MANIFEST_FORMAT {
            return Err(EngineError::Corruption(format!(
                "manifest format tag {:?} unsupported",
                header.format
            )));
        }
        let payload = &bytes[nl + 1..];
        if payload.len() as u64 != header.length {
            return Err(EngineError::Corruption(format!(
                "manifest payload truncated: header says {} bytes, found {}",
                header.length,
                payload.len()
            )));
        }
        let actual = crc32c(payload);
        if actual != header.crc32c {
            return Err(EngineError::Corruption(format!(
                "manifest checksum mismatch: expected {:#010x}, computed {:#010x}",
                header.crc32c, actual
            )));
        }
        let manifest: Manifest = serde_json::from_slice(payload)
            .map_err(|e| EngineError::Corruption(format!("manifest payload unreadable: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// All committed generations, ascending. Authoritative (uses listing, not
    /// the `CURRENT` hint).
    pub async fn list_generations(&self) -> EngineResult<Vec<u64>> {
        let prefix = layout::manifest_prefix(&self.namespace);
        let objects = self.store.list(&prefix).await?;
        let mut gens: Vec<u64> = objects
            .iter()
            .filter_map(|m| layout::parse_manifest_generation(&m.key))
            .collect();
        gens.sort_unstable();
        gens.dedup();
        Ok(gens)
    }

    /// Load a specific generation, `Ok(None)` if it does not exist.
    pub async fn load_generation(&self, generation: u64) -> EngineResult<Option<Manifest>> {
        let key = layout::manifest_key(&self.namespace, generation);
        match self.store.get(&key).await {
            Ok(bytes) => {
                let m = Self::decode(&bytes)?;
                if m.generation != generation {
                    return Err(EngineError::Corruption(format!(
                        "manifest at {key} claims generation {} (expected {generation})",
                        m.generation
                    )));
                }
                if m.namespace != self.namespace {
                    return Err(EngineError::Corruption(format!(
                        "manifest at {key} claims namespace {:?} (expected {:?})",
                        m.namespace, self.namespace
                    )));
                }
                Ok(Some(m))
            }
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Latest decodable manifest plus the list of corrupt generations seen
    /// while walking backwards. `Ok` with `manifest: None` means the
    /// namespace has never been committed.
    pub async fn load_latest_detailed(&self) -> EngineResult<ManifestLoad> {
        let mut gens = self.list_generations().await?;
        let mut corrupted = Vec::new();
        while let Some(gen) = gens.pop() {
            match self.load_generation(gen).await {
                Ok(Some(m)) => {
                    return Ok(ManifestLoad {
                        manifest: Some(m),
                        corrupted_generations: corrupted,
                    });
                }
                Ok(None) => {
                    // Raced with prune; keep walking.
                }
                Err(EngineError::Corruption(msg)) => {
                    tracing::warn!(
                        namespace = %self.namespace,
                        generation = gen,
                        error = %msg,
                        "skipping corrupt manifest generation"
                    );
                    corrupted.push(gen);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(ManifestLoad {
            manifest: None,
            corrupted_generations: corrupted,
        })
    }

    /// Convenience wrapper over [`Self::load_latest_detailed`].
    pub async fn load_latest(&self) -> EngineResult<Option<Manifest>> {
        Ok(self.load_latest_detailed().await?.manifest)
    }

    /// Commit a manifest at its generation with compare-and-set semantics.
    /// Returns [`EngineError::Conflict`] if another writer already committed
    /// this generation; the caller must reload and rebase.
    pub async fn commit(&self, manifest: &Manifest) -> EngineResult<()> {
        manifest.validate()?;
        if manifest.namespace != self.namespace {
            return Err(EngineError::InvalidArgument(format!(
                "manifest namespace {:?} does not match store namespace {:?}",
                manifest.namespace, self.namespace
            )));
        }
        let key = layout::manifest_key(&self.namespace, manifest.generation);
        let bytes = Self::encode(manifest)?;
        let created = self.store.put_if_absent(&key, bytes).await?;
        if !created {
            return Err(EngineError::Conflict(format!(
                "manifest generation {} already committed for namespace {}",
                manifest.generation, self.namespace
            )));
        }
        // Best-effort hint; never authoritative, failure is non-fatal.
        let current_key = layout::current_manifest_key(&self.namespace);
        if let Err(e) = self
            .store
            .put(&current_key, Bytes::from(manifest.generation.to_string()))
            .await
        {
            tracing::warn!(
                namespace = %self.namespace,
                generation = manifest.generation,
                error = %e,
                "failed to update CURRENT manifest hint (non-fatal)"
            );
        }
        Ok(())
    }

    /// Read the advisory `CURRENT` hint, if present and parseable.
    pub async fn read_current_hint(&self) -> Option<u64> {
        let key = layout::current_manifest_key(&self.namespace);
        match self.store.get(&key).await {
            Ok(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok()),
            Err(_) => None,
        }
    }

    /// Delete old manifest generations, keeping the newest `keep_latest`.
    /// Returns the generations deleted. Never deletes below `keep_latest = 1`.
    pub async fn prune(&self, keep_latest: usize) -> EngineResult<Vec<u64>> {
        let keep = keep_latest.max(1);
        let gens = self.list_generations().await?;
        if gens.len() <= keep {
            return Ok(Vec::new());
        }
        let cutoff = gens.len() - keep;
        let mut deleted = Vec::with_capacity(cutoff);
        for &gen in &gens[..cutoff] {
            let key = layout::manifest_key(&self.namespace, gen);
            self.store.delete(&key).await?;
            deleted.push(gen);
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_store::memory::MemoryStore;

    fn store() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    fn sample_segment(id: &str, ns: &str, min: u64, max: u64) -> SegmentRef {
        SegmentRef {
            segment_id: id.to_string(),
            level: 0,
            doc_count: 10,
            live_doc_count: 8,
            tombstone_count: 2,
            size_bytes: 4096,
            min_wal_seq: min,
            max_wal_seq: max,
            created_at_ms: unix_ms(),
            owner_namespace: ns.to_string(),
        }
    }

    #[tokio::test]
    async fn initial_commit_and_load_roundtrip() {
        let ms = ManifestStore::new(store(), "ns-a");
        assert!(ms.load_latest().await.unwrap().is_none());

        let m = Manifest::initial("ns-a", "test-node");
        ms.commit(&m).await.unwrap();

        let loaded = ms.load_latest().await.unwrap().unwrap();
        assert_eq!(loaded, m);
        assert_eq!(ms.read_current_hint().await, Some(1));
    }

    #[tokio::test]
    async fn cas_conflict_on_same_generation() {
        let ms = ManifestStore::new(store(), "ns-a");
        let m = Manifest::initial("ns-a", "writer-1");
        ms.commit(&m).await.unwrap();

        let racer = Manifest::initial("ns-a", "writer-2");
        let err = ms.commit(&racer).await.unwrap_err();
        assert!(matches!(err, EngineError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn successor_generations_advance_state() {
        let ms = ManifestStore::new(store(), "ns-a");
        let g1 = Manifest::initial("ns-a", "n");
        ms.commit(&g1).await.unwrap();

        let mut g2 = g1.next("n");
        g2.wal_head = 12;
        g2.wal_floor = 12;
        g2.segments.push(sample_segment("seg-001", "ns-a", 1, 12));
        ms.commit(&g2).await.unwrap();

        let latest = ms.load_latest().await.unwrap().unwrap();
        assert_eq!(latest.generation, 2);
        assert_eq!(latest.segments.len(), 1);
        assert_eq!(latest.wal_floor, 12);
        assert_eq!(ms.list_generations().await.unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn corrupt_newest_falls_back_to_previous() {
        let s = store();
        let ms = ManifestStore::new(s.clone(), "ns-a");
        let g1 = Manifest::initial("ns-a", "n");
        ms.commit(&g1).await.unwrap();
        let mut g2 = g1.next("n");
        g2.wal_head = 5;
        ms.commit(&g2).await.unwrap();

        // Corrupt generation 2 in place.
        let key = layout::manifest_key("ns-a", 2);
        let mut bytes = s.get(&key).await.unwrap().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        s.delete(&key).await.unwrap();
        s.put(&key, Bytes::from(bytes)).await.unwrap();

        let load = ms.load_latest_detailed().await.unwrap();
        let m = load.manifest.unwrap();
        assert_eq!(m.generation, 1, "should fall back to generation 1");
        assert_eq!(load.corrupted_generations, vec![2]);
    }

    #[tokio::test]
    async fn prune_keeps_latest_generations() {
        let ms = ManifestStore::new(store(), "ns-a");
        let mut m = Manifest::initial("ns-a", "n");
        ms.commit(&m).await.unwrap();
        for _ in 0..4 {
            m = m.next("n");
            ms.commit(&m).await.unwrap();
        }
        assert_eq!(ms.list_generations().await.unwrap(), vec![1, 2, 3, 4, 5]);

        let deleted = ms.prune(2).await.unwrap();
        assert_eq!(deleted, vec![1, 2, 3]);
        assert_eq!(ms.list_generations().await.unwrap(), vec![4, 5]);
        // Load still works after pruning.
        assert_eq!(ms.load_latest().await.unwrap().unwrap().generation, 5);
    }

    #[test]
    fn validate_rejects_inconsistencies() {
        let mut m = Manifest::initial("ns-a", "n");
        m.wal_head = 10;
        m.wal_floor = 10;
        m.segments.push(sample_segment("s1", "ns-a", 1, 10));
        m.validate().unwrap();

        // Duplicate segment id.
        let mut dup = m.clone();
        dup.segments.push(sample_segment("s1", "ns-a", 1, 10));
        assert!(matches!(dup.validate(), Err(EngineError::Corruption(_))));

        // Segment above wal_floor.
        let mut above = m.clone();
        above.segments[0].max_wal_seq = 11;
        assert!(matches!(above.validate(), Err(EngineError::Corruption(_))));

        // wal_floor > wal_head.
        let mut floor = m.clone();
        floor.wal_floor = 11;
        assert!(matches!(floor.validate(), Err(EngineError::Corruption(_))));

        // Row accounting mismatch.
        let mut rows = m;
        rows.segments[0].live_doc_count = 9;
        assert!(matches!(rows.validate(), Err(EngineError::Corruption(_))));
    }

    #[test]
    fn decode_rejects_truncation_and_bitflips() {
        let m = Manifest::initial("ns-a", "n");
        let bytes = ManifestStore::encode(&m).unwrap();

        // Truncated payload.
        let truncated = &bytes[..bytes.len() - 3];
        assert!(matches!(
            ManifestStore::decode(truncated),
            Err(EngineError::Corruption(_))
        ));

        // Single bit flip in the payload.
        let mut flipped = bytes.to_vec();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        assert!(matches!(
            ManifestStore::decode(&flipped),
            Err(EngineError::Corruption(_))
        ));

        // Pristine bytes round-trip.
        assert_eq!(ManifestStore::decode(&bytes).unwrap(), m);
    }
}
