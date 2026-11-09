//! The core storage engine: durable write path, read path, recovery.
//!
//! ## Write path
//!
//! 1. Take the per-namespace write lock (single in-process writer).
//! 2. Idempotency check: a previously seen idempotency key returns the
//!    original `WriteResult` without committing anything.
//! 3. Precondition check (namespace LSN, document existence).
//! 4. Resolve the batch: upserts/deletes map directly to document states;
//!    patches are eagerly resolved against the current merged view
//!    (staged batch -> memtable -> segments) into full documents.
//! 5. Append one WAL record object at `wal/<lsn>` with put-if-absent. This
//!    is the commit point; a conflict means another process owns the LSN.
//! 6. Apply the resolved states to the memtable and record the idempotency
//!    key. Reads observe the write immediately (read-your-writes).
//!
//! ## Recovery (namespace open)
//!
//! 1. Load the current manifest (highest readable version).
//! 2. List WAL record objects; replay records with
//!    `lsn > manifest.last_applied_lsn` in LSN order, re-resolving each
//!    batch exactly like the original write did. This is deterministic
//!    because the manifest's segments capture state as of the watermark.
//! 3. Harvest idempotency keys from *all* surviving WAL records plus the
//!    persisted ledger, so replays after restart remain idempotent.
//! 4. A torn (undecodable) record is tolerated only at the WAL tail — it is
//!    quarantined and its LSN skipped. Corruption mid-stream aborts recovery
//!    and directs the operator to `reef-admin repair`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tracing::{debug, info, warn};

use reef_store::ObjectStore;
use reef_types::write::{Patch, WriteOp};
use reef_types::{DocId, Document};

use crate::error::{EngineError, EngineResult};
use crate::layout;
use crate::manifest::{self, Manifest};
use crate::memtable::{DocState, Memtable};
use crate::now_ms;
use crate::options::EngineOptions;
use crate::segment::{SegmentMeta, SegmentReader};
use crate::wal::{Wal, WalRecord};

// ---------------------------------------------------------------------------
// Public request/response types
// ---------------------------------------------------------------------------

/// A conditional-write precondition, checked atomically with the write under
/// the namespace write lock. If it fails, nothing is committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Precondition {
    /// The namespace's last committed LSN must equal this value (optimistic
    /// concurrency over the whole namespace).
    NamespaceLsnIs(u64),
    /// The given document must currently exist (live, not tombstoned).
    DocExists(DocId),
    /// The given document must not currently exist.
    DocNotExists(DocId),
}

/// A batched write request: any mix of upserts, patches and deletes, with an
/// optional idempotency key and optional precondition.
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub ops: Vec<WriteOp>,
    pub idempotency_key: Option<String>,
    pub precondition: Option<Precondition>,
}

impl WriteRequest {
    pub fn new(ops: Vec<WriteOp>) -> Self {
        Self { ops, idempotency_key: None, precondition: None }
    }

    /// Convenience constructor for a batch of upserts.
    pub fn upsert(docs: Vec<Document>) -> Self {
        Self::new(docs.into_iter().map(WriteOp::Upsert).collect())
    }

    /// Convenience constructor for a batch of deletes by id.
    pub fn delete(ids: Vec<DocId>) -> Self {
        Self::new(ids.into_iter().map(WriteOp::Delete).collect())
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    pub fn with_precondition(mut self, p: Precondition) -> Self {
        self.precondition = Some(p);
        self
    }
}

/// The outcome of a committed (or idempotently replayed) write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResult {
    /// The LSN assigned to the batch.
    pub lsn: u64,
    pub upserts: u64,
    pub patches_applied: u64,
    /// Patches that targeted a document that did not exist; recorded in the
    /// WAL but resolved to no-ops.
    pub patches_missed: u64,
    pub deletes: u64,
    /// True if this result was returned from the idempotency ledger rather
    /// than a fresh commit.
    pub idempotent_replay: bool,
}

/// One remembered idempotency key with the result of its original commit.
/// Persisted to `meta/idempotency.json` whenever the WAL is folded so the
/// idempotency window survives WAL truncation and process restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyEntry {
    pub key: String,
    pub lsn: u64,
    pub committed_at_ms: u64,
    pub result: WriteResult,
}

/// Point-in-time statistics for a namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceStats {
    pub namespace: String,
    pub manifest_version: u64,
    pub last_applied_lsn: u64,
    pub last_committed_lsn: u64,
    /// Document entries in the memtable (WAL records not yet folded).
    pub pending_memtable_entries: usize,
    pub memtable_bytes: u64,
    pub segment_count: usize,
    pub level0_segments: usize,
    pub segment_bytes: u64,
    /// Approximation: segment live counts plus memtable live entries; does
    /// not account for overlap between memtable and segments.
    pub approx_live_docs: u64,
    pub idempotency_entries: usize,
    pub quarantined_wal_files: usize,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Process-local cache of opened segment readers for one namespace. Segments
/// are immutable, so cached readers never go stale; superseded segments are
/// evicted after compaction.
pub(crate) struct SegmentCache {
    readers: std::sync::Mutex<HashMap<String, Arc<SegmentReader>>>,
}

impl SegmentCache {
    pub(crate) fn new() -> Self {
        Self { readers: std::sync::Mutex::new(HashMap::new()) }
    }

    pub(crate) async fn get_or_load(
        &self,
        store: &Arc<dyn ObjectStore>,
        ns: &str,
        meta: &SegmentMeta,
    ) -> EngineResult<Arc<SegmentReader>> {
        if let Some(r) = self.readers.lock().expect("segment cache poisoned").get(&meta.id) {
            return Ok(r.clone());
        }
        let key = layout::segment_key(ns, &meta.id);
        let bytes = store.get(&key).await?;
        let reader = Arc::new(SegmentReader::open(bytes)?);
        self.readers
            .lock()
            .expect("segment cache poisoned")
            .insert(meta.id.clone(), reader.clone());
        Ok(reader)
    }

    pub(crate) fn evict(&self, ids: &[String]) {
        let mut g = self.readers.lock().expect("segment cache poisoned");
        for id in ids {
            g.remove(id);
        }
    }
}

/// Mutable per-namespace state, guarded by a tokio RwLock: queries take read,
/// writes and manifest swaps take write.
pub(crate) struct NsInner {
    pub(crate) manifest: Manifest,
    pub(crate) memtable: Memtable,
    /// Next LSN to assign. Always `max(last_applied_lsn, max replayed WAL
    /// lsn, max quarantined lsn) + 1`.
    pub(crate) next_lsn: u64,
    pub(crate) idempotency: HashMap<String, IdempotencyEntry>,
    /// LSNs of undecodable WAL tail objects discovered at recovery; their
    /// LSNs are burned (never reused) and `repair` deletes the objects.
    pub(crate) quarantined_wal: Vec<u64>,
}

pub(crate) struct NamespaceState {
    pub(crate) name: String,
    pub(crate) inner: RwLock<NsInner>,
    pub(crate) cache: SegmentCache,
    /// Serializes fold/compaction/GC for this namespace within the process.
    pub(crate) maintenance: AsyncMutex<()>,
}

// ---------------------------------------------------------------------------
// Shared resolution helpers (used by the write path, recovery, and admin)
// ---------------------------------------------------------------------------

/// Apply a patch to a document in place: `set` entries overwrite attributes,
/// `unset` removes them. Vectors and text fields are replaced via upsert, not
/// patch (documented engine semantics for v0).
pub(crate) fn apply_patch(doc: &mut Document, patch: &Patch) {
    for (k, v) in &patch.set {
        doc.attributes.insert(k.clone(), v.clone());
    }
    for k in &patch.unset {
        doc.attributes.remove(k);
    }
}

/// Look up the current state of a document across memtable and segments.
/// Segment scan order is newest-first by `max_lsn` with early exit once no
/// remaining segment can contain a newer entry.
pub(crate) async fn lookup_doc(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
    cache: &SegmentCache,
    manifest: &Manifest,
    memtable: &Memtable,
    id: &DocId,
) -> EngineResult<Option<Document>> {
    if let Some(entry) = memtable.get(id) {
        return Ok(match &entry.state {
            DocState::Live(d) => Some(d.clone()),
            DocState::Tombstone => None,
        });
    }
    let mut metas: Vec<&SegmentMeta> = manifest.segments.iter().collect();
    metas.sort_by(|a, b| b.max_lsn.cmp(&a.max_lsn));
    let mut best: Option<(u64, DocState)> = None;
    for meta in metas {
        if let Some((lsn, _)) = &best {
            if *lsn >= meta.max_lsn {
                break; // no remaining segment can hold a newer entry
            }
        }
        let reader = cache.get_or_load(store, ns, meta).await?;
        if let Some(entry) = reader.get(id) {
            let newer = best.as_ref().map_or(true, |(lsn, _)| entry.lsn > *lsn);
            if newer {
                best = Some((entry.lsn, entry.state.clone()));
            }
        }
    }
    Ok(match best {
        Some((_, DocState::Live(d))) => Some(d),
        _ => None,
    })
}

/// The fully resolved effect of a batch: one final state per touched id.
pub(crate) struct ResolvedOps {
    pub(crate) entries: Vec<(DocId, DocState)>,
    pub(crate) upserts: u64,
    pub(crate) patches_applied: u64,
    pub(crate) patches_missed: u64,
    pub(crate) deletes: u64,
}

/// Resolve a batch of ops against the current merged view. Ops apply in
/// order; later ops in the batch observe earlier ones ("staged" view).
/// Patch resolution is *eager*: the merged full document is what lands in
/// the memtable and, eventually, segments. Replaying the same WAL record
/// over the same base state yields an identical result, which makes
/// recovery deterministic.
pub(crate) async fn resolve_ops(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
    cache: &SegmentCache,
    manifest: &Manifest,
    memtable: &Memtable,
    ops: &[WriteOp],
) -> EngineResult<ResolvedOps> {
    let mut staged: BTreeMap<DocId, DocState> = BTreeMap::new();
    let mut upserts = 0u64;
    let mut patches_applied = 0u64;
    let mut patches_missed = 0u64;
    let mut deletes = 0u64;

    for op in ops {
        match op {
            WriteOp::Upsert(doc) => {
                staged.insert(doc.id.clone(), DocState::Live(doc.clone()));
                upserts += 1;
            }
            WriteOp::Delete(id) => {
                staged.insert(id.clone(), DocState::Tombstone);
                deletes += 1;
            }
            WriteOp::Patch(patch) => {
                let base: Option<Document> = match staged.get(&patch.id) {
                    Some(DocState::Live(d)) => Some(d.clone()),
                    Some(DocState::Tombstone) => None,
                    None => lookup_doc(store, ns, cache, manifest, memtable, &patch.id).await?,
                };
                match base {
                    Some(mut doc) => {
                        apply_patch(&mut doc, patch);
                        staged.insert(patch.id.clone(), DocState::Live(doc));
                        patches_applied += 1;
                    }
                    None => {
                        patches_missed += 1;
                    }
                }
            }
        }
    }

    Ok(ResolvedOps {
        entries: staged.into_iter().collect(),
        upserts,
        patches_applied,
        patches_missed,
        deletes,
    })
}

/// Object key of the persisted idempotency ledger for a namespace.
pub(crate) fn ledger_key(ns: &str) -> String {
    let prefix = layout::namespace_prefix(ns);
    format!("{}/meta/idempotency.json", prefix.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// The storage engine. One `Engine` manages many namespaces over a single
/// object store. The engine is cheap to construct; namespaces are opened
/// (recovered) lazily on first access and kept in an in-process registry.
pub struct Engine {
    store: Arc<dyn ObjectStore>,
    opts: EngineOptions,
    namespaces: RwLock<HashMap<String, Arc<NamespaceState>>>,
    open_lock: AsyncMutex<()>,
}

impl Engine {
    pub fn new(store: Arc<dyn ObjectStore>, opts: EngineOptions) -> Self {
        Self {
            store,
            opts,
            namespaces: RwLock::new(HashMap::new()),
            open_lock: AsyncMutex::new(()),
        }
    }

    pub fn options(&self) -> &EngineOptions {
        &self.opts
    }

    pub(crate) fn store_ref(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    // -- namespace lifecycle -------------------------------------------------

    fn validate_namespace_name(ns: &str) -> EngineResult<()> {
        if ns.is_empty() || ns.len() > 128 {
            return Err(EngineError::InvalidArgument(format!(
                "namespace name must be 1..=128 characters, got {} characters",
                ns.len()
            )));
        }
        if !ns.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            return Err(EngineError::InvalidArgument(format!(
                "namespace name '{ns}' may only contain [a-zA-Z0-9._-]"
            )));
        }
        Ok(())
    }

    /// Create a namespace by committing its initial (version 1) manifest.
    /// Creation is atomic: concurrent creators race on put-if-absent of the
    /// manifest object and exactly one wins.
    pub async fn create_namespace(&self, ns: &str) -> EngineResult<()> {
        Self::validate_namespace_name(ns)?;
        if manifest::load_current(&self.store, ns).await?.is_some() {
            return Err(EngineError::AlreadyExists(format!("namespace '{ns}'")));
        }
        let m = Manifest::new(ns);
        match manifest::commit(&self.store, &m).await {
            Ok(()) => {
                info!(namespace = ns, "created namespace");
                Ok(())
            }
            Err(EngineError::Conflict(_)) | Err(EngineError::AlreadyExists(_)) => {
                Err(EngineError::AlreadyExists(format!("namespace '{ns}'")))
            }
            Err(e) => Err(e),
        }
    }

    pub async fn namespace_exists(&self, ns: &str) -> EngineResult<bool> {
        Ok(manifest::load_current(&self.store, ns).await?.is_some())
    }

    /// Delete a namespace and every object under its prefix. Irreversible.
    pub async fn delete_namespace(&self, ns: &str) -> EngineResult<()> {
        self.evict_namespace(ns).await;
        let prefix = layout::namespace_prefix(ns);
        let objects = self.store.list(&prefix).await?;
        if objects.is_empty() {
            return Err(EngineError::NotFound(format!("namespace '{ns}'")));
        }
        for o in &objects {
            self.store.delete(&o.key).await?;
        }
        info!(namespace = ns, objects = objects.len(), "deleted namespace");
        Ok(())
    }

    /// List namespaces by scanning the namespaces root prefix.
    pub async fn list_namespaces(&self) -> EngineResult<Vec<String>> {
        let root = layout::namespaces_root();
        let objects = self.store.list(&root).await?;
        let mut names = BTreeSet::new();
        for o in objects {
            if let Some(rest) = o.key.strip_prefix(&root) {
                let rest = rest.trim_start_matches('/');
                if let Some((first, _)) = rest.split_once('/') {
                    names.insert(first.to_string());
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Names of namespaces currently opened in this process (used by the
    /// maintenance loop).
    pub async fn open_namespaces(&self) -> Vec<String> {
        self.namespaces.read().await.keys().cloned().collect()
    }

    /// Drop the in-process handle for a namespace. The next access re-opens
    /// (recovers) from object storage. Used after admin operations and after
    /// cross-process conflicts to resynchronize.
    pub async fn evict_namespace(&self, ns: &str) {
        if self.namespaces.write().await.remove(ns).is_some() {
            debug!(namespace = ns, "evicted in-process namespace handle");
        }
    }

    // -- handle acquisition & recovery ---------------------------------------

    pub(crate) async fn handle(&self, ns: &str) -> EngineResult<Arc<NamespaceState>> {
        if let Some(h) = self.namespaces.read().await.get(ns) {
            return Ok(h.clone());
        }
        let _g = self.open_lock.lock().await;
        if let Some(h) = self.namespaces.read().await.get(ns) {
            return Ok(h.clone());
        }
        let state = Arc::new(self.open_namespace_state(ns).await?);
        self.namespaces.write().await.insert(ns.to_string(), state.clone());
        Ok(state)
    }

    /// Recover a namespace from object storage: current manifest + WAL replay.
    async fn open_namespace_state(&self, ns: &str) -> EngineResult<NamespaceState> {
        let manifest = manifest::load_current(&self.store, ns)
            .await?
            .ok_or_else(|| EngineError::NotFound(format!("namespace '{ns}'")))?;

        let cache = SegmentCache::new();
        let mut memtable = Memtable::new();
        let mut idempotency = self.load_ledger(ns).await?;
        let mut quarantined: Vec<u64> = Vec::new();

        let wal = Wal::new(self.store.clone(), ns);
        let files = wal.list().await?;
        let mut next_lsn = manifest.last_applied_lsn + 1;
        let total = files.len();
        let mut replayed = 0usize;

        for (i, file) in files.iter().enumerate() {
            let record = match wal.read(file.lsn).await {
                Ok(r) => r,
                Err(EngineError::Corruption(msg)) => {
                    if i == total - 1 {
                        // Torn tail: tolerated, quarantined, LSN burned.
                        warn!(
                            namespace = ns,
                            lsn = file.lsn,
                            error = %msg,
                            "ignoring undecodable WAL tail record (quarantined; run repair to remove)"
                        );
                        quarantined.push(file.lsn);
                        if file.lsn >= next_lsn {
                            next_lsn = file.lsn + 1;
                        }
                        continue;
                    }
                    return Err(EngineError::Corruption(format!(
                        "WAL record at lsn {} in namespace '{ns}' is corrupt and is not the \
                         tail; refusing to skip committed data. Run `reef-admin repair` or \
                         `reef-admin rebuild`. Underlying error: {msg}",
                        file.lsn
                    )));
                }
                Err(e) => return Err(e),
            };

            // Harvest idempotency keys from every surviving record, including
            // already-folded ones whose ledger write may not have landed.
            let mut harvested_result: Option<WriteResult> = None;

            if record.lsn > manifest.last_applied_lsn {
                if record.lsn < next_lsn {
                    warn!(namespace = ns, lsn = record.lsn, "duplicate WAL lsn during replay; skipping");
                    continue;
                }
                if record.lsn > next_lsn {
                    warn!(
                        namespace = ns,
                        expected = next_lsn,
                        got = record.lsn,
                        "gap in WAL lsn sequence during replay; continuing (verify will report)"
                    );
                }
                let resolved =
                    resolve_ops(&self.store, ns, &cache, &manifest, &memtable, &record.ops).await?;
                let result = WriteResult {
                    lsn: record.lsn,
                    upserts: resolved.upserts,
                    patches_applied: resolved.patches_applied,
                    patches_missed: resolved.patches_missed,
                    deletes: resolved.deletes,
                    idempotent_replay: false,
                };
                for (id, state) in resolved.entries {
                    memtable.apply(id, record.lsn, state);
                }
                harvested_result = Some(result);
                next_lsn = record.lsn + 1;
                replayed += 1;
            }

            if let Some(key) = &record.idempotency_key {
                idempotency.entry(key.clone()).or_insert_with(|| IdempotencyEntry {
                    key: key.clone(),
                    lsn: record.lsn,
                    committed_at_ms: record.committed_at_ms,
                    result: harvested_result.unwrap_or(WriteResult {
                        lsn: record.lsn,
                        upserts: 0,
                        patches_applied: 0,
                        patches_missed: 0,
                        deletes: 0,
                        idempotent_replay: false,
                    }),
                });
            }
        }

        info!(
            namespace = ns,
            manifest_version = manifest.version,
            last_applied_lsn = manifest.last_applied_lsn,
            wal_files = total,
            replayed_records = replayed,
            quarantined = quarantined.len(),
            "recovered namespace"
        );

        Ok(NamespaceState {
            name: ns.to_string(),
            inner: RwLock::new(NsInner {
                manifest,
                memtable,
                next_lsn,
                idempotency,
                quarantined_wal: quarantined,
            }),
            cache,
            maintenance: AsyncMutex::new(()),
        })
    }

    // -- write path ----------------------------------------------------------

    /// Commit a batched write. See module docs for the full protocol.
    pub async fn write(&self, ns: &str, req: WriteRequest) -> EngineResult<WriteResult> {
        if req.ops.is_empty() {
            return Err(EngineError::InvalidArgument(
                "write batch must contain at least one operation".to_string(),
            ));
        }
        let h = self.handle(ns).await?;
        let mut inner = h.inner.write().await;

        // 1. Idempotency replay.
        if let Some(key) = &req.idempotency_key {
            if let Some(entry) = inner.idempotency.get(key) {
                let mut result = entry.result.clone();
                result.idempotent_replay = true;
                debug!(namespace = ns, idempotency_key = %key, lsn = result.lsn, "idempotent replay");
                return Ok(result);
            }
        }

        // 2. Precondition.
        if let Some(p) = &req.precondition {
            self.check_precondition(ns, &h, &inner, p).await?;
        }

        // 3. Resolve the batch (may read segments for patch bases).
        let resolved =
            resolve_ops(&self.store, ns, &h.cache, &inner.manifest, &inner.memtable, &req.ops)
                .await?;

        // 4. Durable commit: one WAL record object, created if-absent.
        let lsn = inner.next_lsn;
        let record = WalRecord {
            lsn,
            idempotency_key: req.idempotency_key.clone(),
            ops: req.ops,
            committed_at_ms: now_ms(),
        };
        let wal = Wal::new(self.store.clone(), ns);
        match wal.append(&record).await {
            Ok(_) => {}
            Err(e) if e.is_conflict() => {
                // Another process owns this LSN: our in-memory view is stale.
                drop(inner);
                self.evict_namespace(ns).await;
                return Err(EngineError::Conflict(format!(
                    "WAL append lost the race for lsn {lsn} in namespace '{ns}' \
                     (concurrent writer detected); state resynchronized, please retry: {e}"
                )));
            }
            Err(e) => return Err(e),
        }
        inner.next_lsn = lsn + 1;

        // 5. Make the write visible.
        for (id, state) in resolved.entries {
            inner.memtable.apply(id, lsn, state);
        }

        let result = WriteResult {
            lsn,
            upserts: resolved.upserts,
            patches_applied: resolved.patches_applied,
            patches_missed: resolved.patches_missed,
            deletes: resolved.deletes,
            idempotent_replay: false,
        };
        if let Some(key) = req.idempotency_key {
            inner.idempotency.insert(
                key.clone(),
                IdempotencyEntry {
                    key,
                    lsn,
                    committed_at_ms: now_ms(),
                    result: result.clone(),
                },
            );
        }
        Ok(result)
    }

    async fn check_precondition(
        &self,
        ns: &str,
        h: &NamespaceState,
        inner: &NsInner,
        p: &Precondition,
    ) -> EngineResult<()> {
        match p {
            Precondition::NamespaceLsnIs(want) => {
                let current = inner.next_lsn.saturating_sub(1);
                if current != *want {
                    return Err(EngineError::PreconditionFailed(format!(
                        "namespace '{ns}' is at lsn {current}, precondition requires {want}"
                    )));
                }
            }
            Precondition::DocExists(id) => {
                let found =
                    lookup_doc(&self.store, ns, &h.cache, &inner.manifest, &inner.memtable, id)
                        .await?;
                if found.is_none() {
                    return Err(EngineError::PreconditionFailed(format!(
                        "document '{id:?}' does not exist"
                    )));
                }
            }
            Precondition::DocNotExists(id) => {
                let found =
                    lookup_doc(&self.store, ns, &h.cache, &inner.manifest, &inner.memtable, id)
                        .await?;
                if found.is_some() {
                    return Err(EngineError::PreconditionFailed(format!(
                        "document '{id:?}' already exists"
                    )));
                }
            }
        }
        Ok(())
    }

    // -- read path -----------------------------------------------------------

    /// Point lookup. `Ok(None)` means the document does not exist (or is
    /// tombstoned); a missing namespace is an error.
    pub async fn get_document(&self, ns: &str, id: &DocId) -> EngineResult<Option<Document>> {
        let h = self.handle(ns).await?;
        let inner = h.inner.read().await;
        lookup_doc(&self.store, ns, &h.cache, &inner.manifest, &inner.memtable, id).await
    }

    /// Export every live document, merged across segments and memtable,
    /// sorted by document id.
    pub async fn export(&self, ns: &str) -> EngineResult<Vec<Document>> {
        let h = self.handle(ns).await?;
        let inner = h.inner.read().await;
        let mut merged: BTreeMap<DocId, (u64, DocState)> = BTreeMap::new();
        for meta in &inner.manifest.segments {
            let reader = h.cache.get_or_load(&self.store, ns, meta).await?;
            for entry in reader.iter() {
                let newer = merged.get(&entry.id).map_or(true, |(lsn, _)| entry.lsn > *lsn);
                if newer {
                    merged.insert(entry.id.clone(), (entry.lsn, entry.state.clone()));
                }
            }
        }
        for (id, entry) in inner.memtable.iter() {
            let newer = merged.get(id).map_or(true, |(lsn, _)| entry.lsn > *lsn);
            if newer {
                merged.insert(id.clone(), (entry.lsn, entry.state.clone()));
            }
        }
        Ok(merged
            .into_values()
            .filter_map(|(_, state)| match state {
                DocState::Live(d) => Some(d),
                DocState::Tombstone => None,
            })
            .collect())
    }

    pub async fn stats(&self, ns: &str) -> EngineResult<NamespaceStats> {
        let h = self.handle(ns).await?;
        let inner = h.inner.read().await;
        let segment_bytes: u64 = inner.manifest.segments.iter().map(|s| s.size_bytes).sum();
        let segment_live: u64 = inner
            .manifest
            .segments
            .iter()
            .map(|s| s.doc_count.saturating_sub(s.tombstone_count))
            .sum();
        let memtable_live = inner
            .memtable
            .iter()
            .filter(|(_, e)| matches!(e.state, DocState::Live(_)))
            .count() as u64;
        Ok(NamespaceStats {
            namespace: ns.to_string(),
            manifest_version: inner.manifest.version,
            last_applied_lsn: inner.manifest.last_applied_lsn,
            last_committed_lsn: inner.next_lsn.saturating_sub(1),
            pending_memtable_entries: inner.memtable.len(),
            memtable_bytes: inner.memtable.approx_bytes(),
            segment_count: inner.manifest.segments.len(),
            level0_segments: inner.manifest.segments.iter().filter(|s| s.level == 0).count(),
            segment_bytes,
            approx_live_docs: segment_live + memtable_live,
            idempotency_entries: inner.idempotency.len(),
            quarantined_wal_files: inner.quarantined_wal.len(),
        })
    }

    // -- maintenance entry points (used by indexer loop, CLI, and tests) -----

    /// Fold the memtable into a level-0 segment now. On failure the namespace
    /// handle is evicted so in-process state resynchronizes from storage —
    /// exactly what a crash-restart would do.
    pub async fn fold_now(&self, ns: &str) -> EngineResult<Option<crate::indexer::FoldOutcome>> {
        match crate::indexer::fold_namespace(self, ns).await {
            Ok(o) => Ok(o),
            Err(e) => {
                self.evict_namespace(ns).await;
                Err(e)
            }
        }
    }

    /// Run compaction now (`force` ignores the level-0 trigger). Same
    /// evict-on-failure semantics as [`Engine::fold_now`].
    pub async fn compact_now(
        &self,
        ns: &str,
        force: bool,
    ) -> EngineResult<Option<crate::compaction::CompactionOutcome>> {
        match crate::compaction::compact_namespace(self, ns, force).await {
            Ok(o) => Ok(o),
            Err(e) => {
                self.evict_namespace(ns).await;
                Err(e)
            }
        }
    }

    /// Garbage-collect unreferenced segments, folded WAL records and old
    /// manifest versions.
    pub async fn gc_now(&self, ns: &str) -> EngineResult<crate::compaction::GcReport> {
        match crate::compaction::gc_namespace(self, ns).await {
            Ok(r) => Ok(r),
            Err(e) => {
                self.evict_namespace(ns).await;
                Err(e)
            }
        }
    }

    pub(crate) async fn maintenance_snapshot(&self, ns: &str) -> Option<(usize, u64)> {
        let h = self.namespaces.read().await.get(ns)?.clone();
        let inner = h.inner.read().await;
        Some((inner.memtable.len(), inner.memtable.approx_bytes()))
    }

    // -- idempotency ledger persistence ---------------------------------------

    pub(crate) async fn load_ledger(
        &self,
        ns: &str,
    ) -> EngineResult<HashMap<String, IdempotencyEntry>> {
        match self.store.get(&ledger_key(ns)).await {
            Ok(bytes) => {
                let entries: Vec<IdempotencyEntry> = serde_json::from_slice(&bytes)?;
                Ok(entries.into_iter().map(|e| (e.key.clone(), e)).collect())
            }
            Err(e) if e.is_not_found() => Ok(HashMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn save_ledger(
        &self,
        ns: &str,
        entries: &HashMap<String, IdempotencyEntry>,
    ) -> EngineResult<()> {
        let mut list: Vec<&IdempotencyEntry> = entries.values().collect();
        list.sort_by(|a, b| a.lsn.cmp(&b.lsn));
        let bytes = serde_json::to_vec(&list)?;
        self.store.put(&ledger_key(ns), bytes.into()).await?;
        Ok(())
    }

    /// Apply retention and size caps to an idempotency map in place.
    pub(crate) fn prune_idempotency(&self, map: &mut HashMap<String, IdempotencyEntry>) {
        let cutoff = now_ms().saturating_sub(self.opts.idempotency_retention_ms);
        map.retain(|_, e| e.committed_at_ms >= cutoff);
        if map.len() > self.opts.idempotency_max_entries {
            let mut by_age: Vec<(u64, String)> =
                map.values().map(|e| (e.committed_at_ms, e.key.clone())).collect();
            by_age.sort();
            let excess = map.len() - self.opts.idempotency_max_entries;
            for (_, key) in by_age.into_iter().take(excess) {
                map.remove(&key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use reef_store::memory::MemoryStore;
    use reef_types::Value;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    fn engine(store: &Arc<dyn ObjectStore>) -> Engine {
        Engine::new(store.clone(), EngineOptions::for_tests())
    }

    fn doc(id: &str, n: i64) -> Document {
        let mut attributes = BTreeMap::new();
        attributes.insert("n".to_string(), Value::I64(n));
        Document {
            id: DocId::from(id.to_string()),
            vector: Some(vec![0.1, 0.2, 0.3]),
            sparse_vector: None,
            attributes,
        }
    }

    fn id(s: &str) -> DocId {
        DocId::from(s.to_string())
    }

    #[tokio::test]
    async fn write_and_read_roundtrip() {
        let store = mem();
        let e = engine(&store);
        e.create_namespace("rt").await.unwrap();

        let r = e
            .write("rt", WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)]))
            .await
            .unwrap();
        assert_eq!(r.upserts, 2);
        assert!(!r.idempotent_replay);

        let a = e.get_document("rt", &id("a")).await.unwrap().unwrap();
        assert_eq!(a.attributes.get("n"), Some(&Value::I64(1)));
        assert!(e.get_document("rt", &id("zzz")).await.unwrap().is_none());

        let all = e.export("rt").await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn patch_and_delete_semantics() {
        let store = mem();
        let e = engine(&store);
        e.create_namespace("pd").await.unwrap();
        e.write("pd", WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();

        let mut set = BTreeMap::new();
        set.insert("x".to_string(), Value::I64(7));
        let r = e
            .write(
                "pd",
                WriteRequest::new(vec![
                    WriteOp::Patch(Patch {
                        id: id("a"),
                        set: set.clone(),
                        unset: vec!["n".to_string()],
                    }),
                    WriteOp::Patch(Patch { id: id("missing"), set, unset: vec![] }),
                ]),
            )
            .await
            .unwrap();
        assert_eq!(r.patches_applied, 1);
        assert_eq!(r.patches_missed, 1);

        let a = e.get_document("pd", &id("a")).await.unwrap().unwrap();
        assert_eq!(a.attributes.get("x"), Some(&Value::I64(7)));
        assert!(a.attributes.get("n").is_none());
        assert!(e.get_document("pd", &id("missing")).await.unwrap().is_none());

        e.write("pd", WriteRequest::delete(vec![id("a")])).await.unwrap();
        assert!(e.get_document("pd", &id("a")).await.unwrap().is_none());
        assert!(e.export("pd").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn idempotent_replay_returns_original_result() {
        let store = mem();
        let e = engine(&store);
        e.create_namespace("idem").await.unwrap();

        let req = WriteRequest::upsert(vec![doc("a", 1)]).with_idempotency_key("key-1");
        let first = e.write("idem", req.clone()).await.unwrap();
        let second = e.write("idem", req).await.unwrap();
        assert_eq!(second.lsn, first.lsn);
        assert!(second.idempotent_replay);
        // A second commit never happened: the namespace is still at first.lsn.
        let stats = e.stats("idem").await.unwrap();
        assert_eq!(stats.last_committed_lsn, first.lsn);
    }

    #[tokio::test]
    async fn preconditions_are_enforced() {
        let store = mem();
        let e = engine(&store);
        e.create_namespace("pre").await.unwrap();
        e.write("pre", WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();

        // DocNotExists fails for an existing doc.
        let err = e
            .write(
                "pre",
                WriteRequest::upsert(vec![doc("a", 2)])
                    .with_precondition(Precondition::DocNotExists(id("a"))),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::PreconditionFailed(_)));
        // Failed precondition committed nothing.
        let a = e.get_document("pre", &id("a")).await.unwrap().unwrap();
        assert_eq!(a.attributes.get("n"), Some(&Value::I64(1)));

        // NamespaceLsnIs succeeds with the right value, fails with wrong one.
        let lsn = e.stats("pre").await.unwrap().last_committed_lsn;
        e.write(
            "pre",
            WriteRequest::upsert(vec![doc("b", 2)])
                .with_precondition(Precondition::NamespaceLsnIs(lsn)),
        )
        .await
        .unwrap();
        let err = e
            .write(
                "pre",
                WriteRequest::upsert(vec![doc("c", 3)])
                    .with_precondition(Precondition::NamespaceLsnIs(lsn)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::PreconditionFailed(_)));

        // DocExists.
        let err = e
            .write(
                "pre",
                WriteRequest::upsert(vec![doc("d", 4)])
                    .with_precondition(Precondition::DocExists(id("nope"))),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::PreconditionFailed(_)));
    }

    #[tokio::test]
    async fn restart_recovers_committed_writes_from_wal() {
        let store = mem();
        {
            let e = engine(&store);
            e.create_namespace("crash").await.unwrap();
            e.write("crash", WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)]))
                .await
                .unwrap();
            e.write("crash", WriteRequest::delete(vec![id("b")])).await.unwrap();
            // Engine dropped here without folding: only manifest v1 + WAL exist.
        }
        let e2 = engine(&store);
        let a = e2.get_document("crash", &id("a")).await.unwrap().unwrap();
        assert_eq!(a.attributes.get("n"), Some(&Value::I64(1)));
        assert!(e2.get_document("crash", &id("b")).await.unwrap().is_none());
        let stats = e2.stats("crash").await.unwrap();
        assert_eq!(stats.segment_count, 0);
        assert_eq!(stats.last_committed_lsn, 2);
        assert!(stats.pending_memtable_entries > 0);
        // Writing continues at the next LSN after recovery.
        let r = e2.write("crash", WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();
        assert_eq!(r.lsn, 3);
    }

    #[tokio::test]
    async fn restart_recovers_idempotency_keys_from_wal() {
        let store = mem();
        let original_lsn;
        {
            let e = engine(&store);
            e.create_namespace("idem2").await.unwrap();
            original_lsn = e
                .write(
                    "idem2",
                    WriteRequest::upsert(vec![doc("a", 1)]).with_idempotency_key("once"),
                )
                .await
                .unwrap()
                .lsn;
        }
        let e2 = engine(&store);
        let replay = e2
            .write("idem2", WriteRequest::upsert(vec![doc("a", 1)]).with_idempotency_key("once"))
            .await
            .unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.lsn, original_lsn);
    }

    #[tokio::test]
    async fn fold_then_restart_reads_from_segments_alone() {
        let store = mem();
        {
            let e = engine(&store);
            e.create_namespace("fold").await.unwrap();
            e.write("fold", WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)]))
                .await
                .unwrap();
            let outcome = e.fold_now("fold").await.unwrap().expect("fold should produce a segment");
            assert_eq!(outcome.docs, 2);
            let stats = e.stats("fold").await.unwrap();
            assert_eq!(stats.segment_count, 1);
            assert_eq!(stats.pending_memtable_entries, 0);
        }
        // Restart: WAL was truncated by the fold, so this proves the data is
        // served from the segment referenced by the manifest.
        let e2 = engine(&store);
        let stats = e2.stats("fold").await.unwrap();
        assert_eq!(stats.segment_count, 1);
        assert_eq!(stats.pending_memtable_entries, 0);
        let b = e2.get_document("fold", &id("b")).await.unwrap().unwrap();
        assert_eq!(b.attributes.get("n"), Some(&Value::I64(2)));
        assert_eq!(e2.export("fold").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn namespace_lifecycle() {
        let store = mem();
        let e = engine(&store);
        assert!(!e.namespace_exists("life").await.unwrap());
        e.create_namespace("life").await.unwrap();
        assert!(e.namespace_exists("life").await.unwrap());
        assert!(matches!(
            e.create_namespace("life").await.unwrap_err(),
            EngineError::AlreadyExists(_)
        ));
        assert!(e.list_namespaces().await.unwrap().contains(&"life".to_string()));
        e.write("life", WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.delete_namespace("life").await.unwrap();
        assert!(!e.namespace_exists("life").await.unwrap());
        assert!(e.get_document("life", &id("a")).await.unwrap_err().is_not_found());
        // Bad names are rejected.
        assert!(matches!(
            e.create_namespace("bad/name").await.unwrap_err(),
            EngineError::InvalidArgument(_)
        ));
    }
}
