//! Namespace engine: the layer between the HTTP API and the durable core.
//!
//! Responsibilities:
//! - Namespace lifecycle (create / get / list / update / delete).
//! - The write path: append WAL batches as immutable objects, commit a new
//!   manifest version, trigger background compaction.
//! - Copy-on-write branching and namespace copies (see [`branch`]).
//! - Reference-counted garbage collection of immutable objects shared
//!   between namespaces (see [`refcount`]).
//! - Materialized in-memory namespace state with LRU caching (see [`state`])
//!   and the baseline query executor (see [`query`]).
//! - Cache warming and namespace pinning (see [`warm`]).
//!
//! # Consistency model (single node)
//!
//! All mutations of a namespace are serialized by an in-process per-namespace
//! mutex and committed by writing a new manifest. A write returns only after
//! both the WAL object and the manifest are durably stored, which gives
//! durable writes and read-your-writes on a single node. Multi-node write
//! coordination is out of scope for v1 and documented in the architecture
//! guide.
//!
//! # Crash-safety ordering
//!
//! Every multi-step mutation is ordered so a crash can only *leak* objects,
//! never leave a manifest referencing missing data or a refcount lower than
//! the true reference count:
//!
//! 1. write new immutable objects;
//! 2. increment refcounts for them;
//! 3. commit the manifest;
//! 4. decrement refcounts for objects no longer referenced;
//! 5. delete objects whose refcount reached zero.
//!
//! Leaked objects (crash between steps) are reclaimed by
//! [`Engine::repair_project`], which rebuilds the refcount table from all
//! live manifests and optionally sweeps unreferenced objects.

pub mod branch;
pub mod query;
pub mod refcount;
pub mod state;
pub mod warm;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex, RwLock};

use shoal_cache::layer::CacheLayer;
use crate::filter::Filter;
use crate::layout;
use crate::manifest::{Manifest, SegmentRef, WalRef};
use crate::segment;
use crate::storage::ObjectStore;
use crate::types::{AttributeValue, DistanceMetric, Document, SparseVector};
use crate::wal::{WalBatch, WalRecord};

use self::refcount::RefCounter;
use self::state::StateCache;
use self::warm::PinSet;

/// Shared handle to the object store.
pub type Store = Arc<dyn ObjectStore>;

/// Hard cap on documents per write batch (protects memory and WAL object size).
pub const MAX_WRITE_BATCH: usize = 10_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("namespace not found: {0}")]
    NamespaceNotFound(String),
    #[error("namespace already exists: {0}")]
    NamespaceExists(String),
    #[error("document not found: {0}")]
    DocumentNotFound(String),
    #[error("version conflict: expected manifest version {expected}, found {actual}")]
    VersionConflict { expected: u64, actual: u64 },
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("internal error: {0}")]
    Core(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

pub(crate) fn core_err<E: std::fmt::Display>(e: E) -> EngineError {
    EngineError::Core(e.to_string())
}

pub(crate) fn storage_err<E: std::fmt::Display>(e: E) -> EngineError {
    EngineError::Storage(e.to_string())
}

// ---------------------------------------------------------------------------
// Namespace path
// ---------------------------------------------------------------------------

/// A fully qualified namespace path: organization / project / namespace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NsPath {
    pub org: String,
    pub project: String,
    pub ns: String,
}

impl NsPath {
    pub fn new(org: &str, project: &str, ns: &str) -> Result<Self> {
        validate_component(org, "organization")?;
        validate_component(project, "project")?;
        validate_component(ns, "namespace")?;
        Ok(NsPath {
            org: org.to_string(),
            project: project.to_string(),
            ns: ns.to_string(),
        })
    }

    /// Parse `org/project/namespace`.
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 3 {
            return Err(EngineError::Invalid(format!(
                "namespace path must be org/project/namespace, got {s:?}"
            )));
        }
        NsPath::new(parts[0], parts[1], parts[2])
    }

    /// Root object-key prefix for this namespace.
    pub fn root(&self) -> String {
        layout::namespace_root(&self.org, &self.project, &self.ns)
    }

    /// Human-readable path, `org/project/namespace`.
    pub fn display(&self) -> String {
        format!("{}/{}/{}", self.org, self.project, self.ns)
    }
}

/// Validate a path component. Components become object-key path segments, so
/// this is also our path-traversal defense: a strict allowlist.
pub fn validate_component(s: &str, what: &str) -> Result<()> {
    if s.is_empty() || s.len() > 128 {
        return Err(EngineError::Invalid(format!(
            "{what} must be 1-128 characters, got {} characters",
            s.len()
        )));
    }
    if s.starts_with('_') || s.starts_with('.') || s.starts_with('-') {
        return Err(EngineError::Invalid(format!(
            "{what} must not start with '_', '.' or '-': {s:?}"
        )));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(EngineError::Invalid(format!(
            "{what} may only contain ASCII letters, digits, '-', '_', '.': {s:?}"
        )));
    }
    if s.contains("..") {
        return Err(EngineError::Invalid(format!(
            "{what} must not contain '..': {s:?}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public DTOs
// ---------------------------------------------------------------------------

fn default_metric() -> DistanceMetric {
    DistanceMetric::Cosine
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateNamespaceOptions {
    #[serde(default = "default_metric")]
    pub distance_metric: DistanceMetric,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl Default for CreateNamespaceOptions {
    fn default() -> Self {
        CreateNamespaceOptions {
            distance_metric: default_metric(),
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WriteOptions {
    /// Conditional write: only apply if the manifest is at exactly this version.
    #[serde(default)]
    pub if_version: Option<u64>,
    /// Idempotency key. Re-submitting a write with the same key returns the
    /// stored result of the first successful application.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WriteResult {
    /// Manifest version after the write committed.
    pub version: u64,
    pub first_seq: u64,
    pub last_seq: u64,
    pub records: u64,
    /// True if this request was deduplicated via its idempotency key.
    pub deduplicated: bool,
}

/// Partial update of an existing document. Provided fields replace or merge
/// into the stored document; `attrs` entries with JSON `null` values remove
/// the corresponding attribute.
#[derive(Debug, Deserialize)]
pub struct DocumentPatch {
    pub id: String,
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    #[serde(default)]
    pub sparse: Option<SparseVector>,
    #[serde(default)]
    pub text: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub attrs: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
pub struct NamespaceInfo {
    pub org: String,
    pub project: String,
    pub namespace: String,
    pub version: u64,
    pub doc_count: u64,
    pub segments: usize,
    pub wal_files: usize,
    pub wal_records: u64,
    pub bytes: u64,
    pub distance_metric: DistanceMetric,
    pub vector_dim: Option<u32>,
    pub parent: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub pinned: bool,
}

#[derive(Debug, Serialize)]
pub struct CompactionReport {
    pub performed: bool,
    pub live_documents: u64,
    pub segment_bytes: u64,
    pub removed_objects: usize,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct RepairReport {
    pub manifests: usize,
    pub tracked_objects: usize,
    pub swept_objects: usize,
}

/// Idempotency marker stored next to the namespace.
#[derive(Debug, Serialize, Deserialize)]
struct StoredWrite {
    version: u64,
    first_seq: u64,
    last_seq: u64,
    records: u64,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    /// Compact once a namespace accumulates this many WAL objects.
    #[serde(default = "default_wal_compact_threshold")]
    pub wal_compact_threshold: usize,
    /// Compact once a namespace accumulates more than this many segments.
    #[serde(default = "default_segment_compact_threshold")]
    pub segment_compact_threshold: usize,
    /// Maximum number of materialized namespace states held in memory.
    #[serde(default = "default_state_cache_entries")]
    pub state_cache_entries: usize,
    /// Approximate memory budget for materialized states, in bytes.
    #[serde(default = "default_state_cache_bytes")]
    pub state_cache_bytes: usize,
    /// Whether writes automatically schedule background compaction.
    #[serde(default = "default_true")]
    pub auto_compact: bool,
}

fn default_wal_compact_threshold() -> usize {
    8
}
fn default_segment_compact_threshold() -> usize {
    4
}
fn default_state_cache_entries() -> usize {
    64
}
fn default_state_cache_bytes() -> usize {
    512 * 1024 * 1024
}
fn default_true() -> bool {
    true
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            wal_compact_threshold: default_wal_compact_threshold(),
            segment_compact_threshold: default_segment_compact_threshold(),
            state_cache_entries: default_state_cache_entries(),
            state_cache_bytes: default_state_cache_bytes(),
            auto_compact: default_true(),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct Engine {
    pub(crate) store: Store,
    pub(crate) cache: Arc<CacheLayer>,
    pub(crate) refs: RefCounter,
    pub(crate) cfg: EngineConfig,
    /// Per-namespace mutation locks (serialize all manifest changes).
    ns_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Per-namespace state-build locks (prevent rebuild stampedes without
    /// blocking writers).
    build_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Hot in-memory manifest cache (the memory tier for metadata).
    manifests: RwLock<HashMap<String, Arc<Manifest>>>,
    /// LRU cache of materialized namespace states.
    pub(crate) states: StateCache,
    /// Persistent set of pinned namespaces.
    pub(crate) pins: PinSet,
    compact_tx: mpsc::UnboundedSender<NsPath>,
    compact_rx: Mutex<Option<mpsc::UnboundedReceiver<NsPath>>>,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Join an object-key prefix and a relative suffix with exactly one slash.
pub(crate) fn join_key(prefix: &str, rest: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        rest.trim_start_matches('/')
    )
}

impl Engine {
    pub fn new(store: Store, cache: Arc<CacheLayer>, cfg: EngineConfig) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Engine {
            refs: RefCounter::new(store.clone()),
            pins: PinSet::new(store.clone()),
            states: StateCache::new(cfg.state_cache_entries, cfg.state_cache_bytes),
            ns_locks: Mutex::new(HashMap::new()),
            build_locks: Mutex::new(HashMap::new()),
            manifests: RwLock::new(HashMap::new()),
            compact_tx: tx,
            compact_rx: Mutex::new(Some(rx)),
            store,
            cache,
            cfg,
        })
    }

    /// Restore pins from durable storage, re-pin and re-warm them, and start
    /// the background compaction worker. Call once on startup.
    pub async fn initialize(self: &Arc<Self>) -> Result<()> {
        self.pins.load().await?;
        for path_str in self.pins.all() {
            match NsPath::parse(&path_str) {
                Ok(p) => {
                    self.cache.pin_prefix(&p.root());
                    let eng = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = eng.warm_namespace(&p, true).await {
                            tracing::warn!(namespace = %p.display(), error = %e,
                                "failed to warm pinned namespace on startup");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(pin = %path_str, error = %e, "ignoring invalid pin entry");
                }
            }
        }
        self.spawn_compactor();
        Ok(())
    }

    fn spawn_compactor(self: &Arc<Self>) {
        let rx = match self.compact_rx.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        let Some(mut rx) = rx else {
            return; // already spawned
        };
        let eng = self.clone();
        tokio::spawn(async move {
            while let Some(path) = rx.recv().await {
                match eng.compact(&path, false).await {
                    Ok(report) if report.performed => {
                        tracing::info!(
                            namespace = %path.display(),
                            live_documents = report.live_documents,
                            removed_objects = report.removed_objects,
                            version = report.version,
                            "background compaction completed"
                        );
                    }
                    Ok(_) => {}
                    Err(EngineError::NamespaceNotFound(_)) => {} // deleted meanwhile
                    Err(e) => {
                        tracing::warn!(namespace = %path.display(), error = %e,
                            "background compaction failed");
                    }
                }
            }
        });
    }

    pub fn cache_layer(&self) -> &Arc<CacheLayer> {
        &self.cache
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn object_store(&self) -> &Store {
        &self.store
    }

    // -- Locks --------------------------------------------------------------

    pub(crate) async fn ns_lock(&self, root: &str) -> Arc<Mutex<()>> {
        self.ns_locks
            .lock()
            .await
            .entry(root.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub(crate) async fn build_lock(&self, root: &str) -> Arc<Mutex<()>> {
        self.build_locks
            .lock()
            .await
            .entry(root.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    // -- Manifest IO ----------------------------------------------------------

    fn manifest_key_for(path: &NsPath) -> String {
        layout::manifest_key(&path.root())
    }

    /// Read the manifest, preferring the in-memory hot cache.
    pub(crate) async fn load_manifest(&self, path: &NsPath) -> Result<Arc<Manifest>> {
        if let Some(m) = self.manifests.read().await.get(&path.root()) {
            return Ok(m.clone());
        }
        self.load_manifest_from_store(path).await
    }

    /// Read the manifest directly from the object store, refreshing the cache.
    pub(crate) async fn load_manifest_from_store(&self, path: &NsPath) -> Result<Arc<Manifest>> {
        let key = Self::manifest_key_for(path);
        let exists = self.store.exists(&key).await.map_err(storage_err)?;
        if !exists {
            return Err(EngineError::NamespaceNotFound(path.display()));
        }
        let bytes = self.store.get(&key).await.map_err(storage_err)?;
        let manifest = Manifest::from_bytes(&bytes).map_err(core_err)?;
        let manifest = Arc::new(manifest);
        self.manifests
            .write()
            .await
            .insert(path.root(), manifest.clone());
        Ok(manifest)
    }

    /// Durably write the manifest and refresh the hot cache. Callers must
    /// hold the namespace lock.
    pub(crate) async fn commit_manifest(
        &self,
        path: &NsPath,
        manifest: Manifest,
    ) -> Result<Arc<Manifest>> {
        let key = Self::manifest_key_for(path);
        let bytes = manifest.to_bytes().map_err(core_err)?;
        self.store
            .put(&key, Bytes::from(bytes))
            .await
            .map_err(storage_err)?;
        let manifest = Arc::new(manifest);
        self.manifests
            .write()
            .await
            .insert(path.root(), manifest.clone());
        Ok(manifest)
    }

    pub(crate) fn info(&self, path: &NsPath, m: &Manifest) -> NamespaceInfo {
        NamespaceInfo {
            org: path.org.clone(),
            project: path.project.clone(),
            namespace: path.ns.clone(),
            version: m.version,
            doc_count: m.doc_count,
            segments: m.segments.len(),
            wal_files: m.wal.len(),
            wal_records: m.wal_records(),
            bytes: m.total_bytes(),
            distance_metric: m.distance_metric.clone(),
            vector_dim: m.vector_dim,
            parent: m.parent.clone(),
            labels: m.labels.clone(),
            created_at_ms: m.created_at_ms,
            updated_at_ms: m.updated_at_ms,
            pinned: self.pins.contains(&path.display()),
        }
    }

    // -- Namespace lifecycle ----------------------------------------------------

    pub async fn create_namespace(
        &self,
        path: &NsPath,
        opts: CreateNamespaceOptions,
    ) -> Result<NamespaceInfo> {
        let root = path.root();
        let lock = self.ns_lock(&root).await;
        let _guard = lock.lock().await;
        let key = Self::manifest_key_for(path);
        if self.store.exists(&key).await.map_err(storage_err)? {
            return Err(EngineError::NamespaceExists(path.display()));
        }
        let mut manifest = Manifest::new(&path.display(), opts.distance_metric);
        manifest.labels = opts.labels;
        let committed = self.commit_manifest(path, manifest).await?;
        Ok(self.info(path, &committed))
    }

    pub async fn get_namespace(&self, path: &NsPath) -> Result<NamespaceInfo> {
        let m = self.load_manifest(path).await?;
        Ok(self.info(path, &m))
    }

    pub async fn namespace_exists(&self, path: &NsPath) -> Result<bool> {
        let key = Self::manifest_key_for(path);
        self.store.exists(&key).await.map_err(storage_err)
    }

    pub async fn list_namespaces(&self, org: &str, project: &str) -> Result<Vec<String>> {
        validate_component(org, "organization")?;
        validate_component(project, "project")?;
        let prefix = join_key(&layout::project_root(org, project), "namespaces/");
        let keys = self.store.list(&prefix).await.map_err(storage_err)?;
        let mut out: Vec<String> = keys
            .into_iter()
            .filter_map(|k| {
                let rest = k.strip_prefix(&prefix)?;
                let (ns, tail) = rest.split_once('/')?;
                if tail == "manifest.json" {
                    Some(ns.to_string())
                } else {
                    None
                }
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Replace namespace labels.
    pub async fn update_namespace(
        &self,
        path: &NsPath,
        labels: BTreeMap<String, String>,
    ) -> Result<NamespaceInfo> {
        let lock = self.ns_lock(&path.root()).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest(path).await?;
        let mut next = (*m).clone();
        next.labels = labels;
        next.version += 1;
        next.updated_at_ms = now_ms();
        let committed = self.commit_manifest(path, next).await?;
        Ok(self.info(path, &committed))
    }

    /// Delete a namespace. Shared objects (referenced by branches or copies)
    /// survive via reference counting; exclusively owned objects are removed.
    pub async fn delete_namespace(&self, path: &NsPath) -> Result<()> {
        let root = path.root();
        let lock = self.ns_lock(&root).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest_from_store(path).await?;
        let keys = m.object_keys();

        // Remove the manifest first: the namespace disappears as a unit, and
        // a crash after this point only leaks objects (repair reclaims them).
        let manifest_key = Self::manifest_key_for(path);
        self.store
            .delete(&manifest_key)
            .await
            .map_err(storage_err)?;
        self.manifests.write().await.remove(&root);
        self.states.invalidate(&root);

        if self.pins.contains(&path.display()) {
            let _ = self.pins.unpin(&path.display()).await;
            self.cache.unpin_prefix(&root);
        }

        let zeroed = self.refs.decr(&path.org, &path.project, &keys).await?;
        for key in zeroed {
            if let Err(e) = self.store.delete(&key).await {
                tracing::warn!(key = %key, error = %e, "failed to delete unreferenced object");
            }
            self.cache.invalidate(&key);
        }

        // Sweep namespace-local idempotency markers.
        let idem_prefix = join_key(&root, "_idem/");
        if let Ok(markers) = self.store.list(&idem_prefix).await {
            for key in markers {
                let _ = self.store.delete(&key).await;
            }
        }
        Ok(())
    }

    // -- Write path ------------------------------------------------------------

    pub async fn upsert(
        &self,
        path: &NsPath,
        docs: Vec<Document>,
        opts: WriteOptions,
    ) -> Result<WriteResult> {
        if docs.is_empty() {
            return Err(EngineError::Invalid(
                "upsert requires at least one document".into(),
            ));
        }
        if docs.len() > MAX_WRITE_BATCH {
            return Err(EngineError::Invalid(format!(
                "write batch exceeds maximum of {MAX_WRITE_BATCH} documents"
            )));
        }
        for d in &docs {
            if d.id.is_empty() {
                return Err(EngineError::Invalid("document id must not be empty".into()));
            }
        }
        let lock = self.ns_lock(&path.root()).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest(path).await?;

        // Enforce / pin vector dimensionality.
        let mut dim = m.vector_dim;
        for d in &docs {
            if let Some(v) = &d.vector {
                match dim {
                    None => dim = Some(v.len() as u32),
                    Some(expected) if expected as usize != v.len() => {
                        return Err(EngineError::Invalid(format!(
                            "vector dimension mismatch: namespace uses {expected}, document {:?} has {}",
                            d.id,
                            v.len()
                        )));
                    }
                    _ => {}
                }
            }
        }

        let count = docs.len() as i64;
        let records: Vec<WalRecord> = docs.into_iter().map(WalRecord::Upsert).collect();
        self.append_inner(path, &m, records, &opts, dim, count)
            .await
    }

    pub async fn patch(
        &self,
        path: &NsPath,
        patches: Vec<DocumentPatch>,
        opts: WriteOptions,
    ) -> Result<WriteResult> {
        if patches.is_empty() {
            return Err(EngineError::Invalid(
                "patch requires at least one document patch".into(),
            ));
        }
        if patches.len() > MAX_WRITE_BATCH {
            return Err(EngineError::Invalid(format!(
                "write batch exceeds maximum of {MAX_WRITE_BATCH} documents"
            )));
        }
        let lock = self.ns_lock(&path.root()).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest(path).await?;
        let st = self.namespace_state(path).await?;

        let mut dim = m.vector_dim;
        let mut records = Vec::with_capacity(patches.len());
        for p in patches {
            let Some(existing) = st.get(&p.id) else {
                return Err(EngineError::DocumentNotFound(p.id));
            };
            let mut doc = existing.clone();
            if let Some(v) = p.vector {
                match dim {
                    None => dim = Some(v.len() as u32),
                    Some(expected) if expected as usize != v.len() => {
                        return Err(EngineError::Invalid(format!(
                            "vector dimension mismatch: namespace uses {expected}, patch for {:?} has {}",
                            doc.id,
                            v.len()
                        )));
                    }
                    _ => {}
                }
                doc.vector = Some(v);
            }
            if let Some(sv) = p.sparse {
                doc.sparse_vector = Some(sv);
            }
            if let Some(text) = p.text {
                for (k, v) in text {
                    doc.text.insert(k, v);
                }
            }
            if let Some(attrs) = p.attrs {
                for (k, v) in attrs {
                    if v.is_null() {
                        doc.attributes.remove(&k);
                    } else {
                        // Convert serde_json::Value → AttributeValue
                        let av = match v {
                            Value::Bool(b) => AttributeValue::Bool(b),
                            Value::Number(n) => {
                                if let Some(i) = n.as_i64() {
                                    AttributeValue::Int(i)
                                } else {
                                    AttributeValue::Float(n.as_f64().unwrap_or(0.0))
                                }
                            }
                            Value::String(s) => AttributeValue::String(s),
                            Value::Array(arr) => AttributeValue::StringList(
                                arr.into_iter()
                                    .filter_map(|x| x.as_str().map(String::from))
                                    .collect(),
                            ),
                            _ => AttributeValue::Null,
                        };
                        doc.attributes.insert(k, av);
                    }
                }
            }
            records.push(WalRecord::Upsert(doc));
        }
        self.append_inner(path, &m, records, &opts, dim, 0).await
    }

    pub async fn delete_documents(
        &self,
        path: &NsPath,
        ids: Vec<String>,
        opts: WriteOptions,
    ) -> Result<WriteResult> {
        if ids.is_empty() {
            return Err(EngineError::Invalid(
                "delete requires at least one document id".into(),
            ));
        }
        if ids.len() > MAX_WRITE_BATCH {
            return Err(EngineError::Invalid(format!(
                "write batch exceeds maximum of {MAX_WRITE_BATCH} documents"
            )));
        }
        for id in &ids {
            if id.is_empty() {
                return Err(EngineError::Invalid("document id must not be empty".into()));
            }
        }
        let lock = self.ns_lock(&path.root()).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest(path).await?;
        let count = ids.len() as i64;
        let records: Vec<WalRecord> = ids
            .into_iter()
            .map(|id| WalRecord::Delete { id })
            .collect();
        self.append_inner(path, &m, records, &opts, None, -count)
            .await
    }

    /// Delete all documents matching `filter`. Returns the number of deleted
    /// documents and the write result (None when nothing matched).
    pub async fn delete_by_filter(
        &self,
        path: &NsPath,
        filter: &Filter,
        opts: WriteOptions,
    ) -> Result<(u64, Option<WriteResult>)> {
        let lock = self.ns_lock(&path.root()).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest(path).await?;
        let st = self.namespace_state(path).await?;
        let ids: Vec<String> = st
            .docs
            .iter()
            .filter(|d| filter.matches(d))
            .map(|d| d.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok((0, None));
        }
        let count = ids.len() as i64;
        let records: Vec<WalRecord> = ids
            .into_iter()
            .map(|id| WalRecord::Delete { id })
            .collect();
        let result = self
            .append_inner(path, &m, records, &opts, None, -count)
            .await?;
        Ok((count as u64, Some(result)))
    }

    fn idem_marker_key(path: &NsPath, idempotency_key: &str) -> String {
        let token = idem_token(idempotency_key);
        join_key(&path.root(), &format!("_idem/{token}.json"))
    }

    /// Core write commit. Caller must hold the namespace lock and pass the
    /// manifest read under that lock.
    async fn append_inner(
        &self,
        path: &NsPath,
        current: &Manifest,
        records: Vec<WalRecord>,
        opts: &WriteOptions,
        vector_dim: Option<u32>,
        doc_delta: i64,
    ) -> Result<WriteResult> {
        if let Some(expected) = opts.if_version {
            if expected != current.version {
                return Err(EngineError::VersionConflict {
                    expected,
                    actual: current.version,
                });
            }
        }

        // Idempotency: replay the stored result if this key already applied.
        if let Some(idk) = &opts.idempotency_key {
            let marker = Self::idem_marker_key(path, idk);
            if self.store.exists(&marker).await.map_err(storage_err)? {
                let bytes = self.store.get(&marker).await.map_err(storage_err)?;
                let stored: StoredWrite = serde_json::from_slice(&bytes).map_err(core_err)?;
                return Ok(WriteResult {
                    version: stored.version,
                    first_seq: stored.first_seq,
                    last_seq: stored.last_seq,
                    records: stored.records,
                    deduplicated: true,
                });
            }
        }

        let first_seq = current.next_seq;
        let count = records.len() as u64;
        let last_seq = first_seq + count - 1;
        let batch = WalBatch::new(first_seq, records);
        let data = batch.encode().map_err(core_err)?;
        let size_bytes = data.len() as u64;

        let token = opts
            .idempotency_key
            .as_deref()
            .map(idem_token)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let wal_key = join_key(
            &path.root(),
            &format!("wal/{first_seq:020}-{token}.wal"),
        );

        // 1. immutable object, 2. refcount, 3. manifest (leak-only ordering).
        self.store
            .put(&wal_key, Bytes::from(data))
            .await
            .map_err(storage_err)?;
        self.refs
            .incr(&path.org, &path.project, std::slice::from_ref(&wal_key))
            .await?;

        let mut next = current.clone();
        next.wal.push(WalRef {
            key: wal_key,
            first_seq,
            last_seq,
            record_count: count,
            size_bytes,
        });
        next.next_seq = last_seq + 1;
        next.version += 1;
        next.updated_at_ms = now_ms();
        if vector_dim.is_some() {
            next.vector_dim = vector_dim;
        }
        next.doc_count = if doc_delta >= 0 {
            next.doc_count.saturating_add(doc_delta as u64)
        } else {
            next.doc_count.saturating_sub(doc_delta.unsigned_abs())
        };
        let wal_files = next.wal.len();
        let committed = self.commit_manifest(path, next).await?;
        self.states.invalidate(&path.root());

        let result = WriteResult {
            version: committed.version,
            first_seq,
            last_seq,
            records: count,
            deduplicated: false,
        };

        if let Some(idk) = &opts.idempotency_key {
            let marker = Self::idem_marker_key(path, idk);
            let stored = StoredWrite {
                version: result.version,
                first_seq,
                last_seq,
                records: count,
            };
            match serde_json::to_vec(&stored) {
                Ok(bytes) => {
                    if let Err(e) = self.store.put(&marker, Bytes::from(bytes)).await {
                        tracing::warn!(error = %e, "failed to persist idempotency marker");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to encode idempotency marker"),
            }
        }

        if self.cfg.auto_compact && wal_files >= self.cfg.wal_compact_threshold {
            let _ = self.compact_tx.send(path.clone());
        }
        Ok(result)
    }

    // -- Materialized state ------------------------------------------------------

    /// Get the materialized state for a namespace, building it from object
    /// storage (through the cache hierarchy) if not already in memory.
    pub async fn namespace_state(&self, path: &NsPath) -> Result<Arc<state::NamespaceState>> {
        let m = self.load_manifest(path).await?;
        let root = path.root();
        if let Some(st) = self.states.get(&root, m.version) {
            return Ok(st);
        }
        let build_lock = self.build_lock(&root).await;
        let _guard = build_lock.lock().await;
        // Re-check after winning the build race.
        if let Some(st) = self.states.get(&root, m.version) {
            return Ok(st);
        }
        let docs = state::materialize(self, &m).await?;
        let st = Arc::new(state::NamespaceState::new(m.version, docs));
        self.states
            .put(&root, st.clone(), self.pins.contains(&path.display()));
        Ok(st)
    }

    // -- Compaction ---------------------------------------------------------------

    /// Fold all WAL batches and segments into a single fresh segment. With
    /// `force == false` this is a no-op below the configured thresholds.
    pub async fn compact(&self, path: &NsPath, force: bool) -> Result<CompactionReport> {
        let root = path.root();
        let lock = self.ns_lock(&root).await;
        let _guard = lock.lock().await;
        let m = self.load_manifest_from_store(path).await?;

        let needs = m.wal.len() >= self.cfg.wal_compact_threshold
            || m.segments.len() > self.cfg.segment_compact_threshold;
        if !force && !needs {
            return Ok(CompactionReport {
                performed: false,
                live_documents: m.doc_count,
                segment_bytes: 0,
                removed_objects: 0,
                version: m.version,
            });
        }
        if m.wal.is_empty() && m.segments.len() <= 1 && !force {
            return Ok(CompactionReport {
                performed: false,
                live_documents: m.doc_count,
                segment_bytes: 0,
                removed_objects: 0,
                version: m.version,
            });
        }

        let docs = state::materialize(self, &m).await?;
        let old_keys = m.object_keys();
        let mut next = (*m).clone();
        next.wal.clear();

        let mut segment_bytes = 0u64;
        if docs.is_empty() {
            next.segments.clear();
        } else {
            let bytes = segment::build_segment(&docs).map_err(core_err)?;
            segment_bytes = bytes.len() as u64;
            let seg_id = uuid::Uuid::new_v4().simple().to_string();
            let seg_key = join_key(&root, &format!("segments/{seg_id}.seg"));
            self.store
                .put(&seg_key, Bytes::from(bytes))
                .await
                .map_err(storage_err)?;
            self.refs
                .incr(&path.org, &path.project, std::slice::from_ref(&seg_key))
                .await?;
            next.segments = vec![SegmentRef {
                id: seg_id,
                key: seg_key,
                doc_count: docs.len() as u64,
                size_bytes: segment_bytes,
                created_at_ms: now_ms(),
            }];
        }
        next.doc_count = docs.len() as u64;
        next.version += 1;
        next.updated_at_ms = now_ms();
        let committed = self.commit_manifest(path, next).await?;
        self.states.invalidate(&root);

        let zeroed = self.refs.decr(&path.org, &path.project, &old_keys).await?;
        let removed = zeroed.len();
        for key in zeroed {
            if let Err(e) = self.store.delete(&key).await {
                tracing::warn!(key = %key, error = %e, "failed to delete compacted object");
            }
            self.cache.invalidate(&key);
        }

        Ok(CompactionReport {
            performed: true,
            live_documents: docs.len() as u64,
            segment_bytes,
            removed_objects: removed,
            version: committed.version,
        })
    }

    // -- Repair ---------------------------------------------------------------------

    /// Rebuild the project refcount table from all live manifests, and
    /// optionally sweep unreferenced data objects. Run during maintenance
    /// windows: a sweep racing an in-flight write could remove an object
    /// written but not yet committed to a manifest.
    pub async fn repair_project(
        &self,
        org: &str,
        project: &str,
        sweep: bool,
    ) -> Result<RepairReport> {
        let namespaces = self.list_namespaces(org, project).await?;
        let mut manifests = Vec::with_capacity(namespaces.len());
        for ns in &namespaces {
            let path = NsPath::new(org, project, ns)?;
            match self.load_manifest_from_store(&path).await {
                Ok(m) => manifests.push((*m).clone()),
                Err(EngineError::NamespaceNotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let table = self.refs.rebuild(org, project, manifests.iter()).await?;

        let mut swept = 0usize;
        if sweep {
            let prefix = join_key(&layout::project_root(org, project), "namespaces/");
            let keys = self.store.list(&prefix).await.map_err(storage_err)?;
            for key in keys {
                if key.ends_with("/manifest.json") {
                    continue;
                }
                if key.contains("/_idem/") {
                    continue;
                }
                if table.counts.contains_key(&key) {
                    continue;
                }
                if let Err(e) = self.store.delete(&key).await {
                    tracing::warn!(key = %key, error = %e, "sweep delete failed");
                    continue;
                }
                self.cache.invalidate(&key);
                swept += 1;
            }
        }
        Ok(RepairReport {
            manifests: manifests.len(),
            tracked_objects: table.counts.len(),
            swept_objects: swept,
        })
    }
}

fn idem_token(idempotency_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(idempotency_key.as_bytes());
    hex::encode(&hasher.finalize()[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_validation() {
        assert!(NsPath::new("acme", "search", "docs").is_ok());
        assert!(NsPath::new("acme", "search", "docs-v1.2_x").is_ok());
        assert!(NsPath::new("", "p", "n").is_err());
        assert!(NsPath::new("a/b", "p", "n").is_err());
        assert!(NsPath::new("_meta", "p", "n").is_err());
        assert!(NsPath::new(".hidden", "p", "n").is_err());
        assert!(NsPath::new("a..b", "p", "n").is_err());
        assert!(NsPath::new(&"x".repeat(129), "p", "n").is_err());
    }

    #[test]
    fn path_parse_roundtrip() {
        let p = NsPath::parse("acme/search/docs").unwrap();
        assert_eq!(p.display(), "acme/search/docs");
        assert!(NsPath::parse("a/b").is_err());
        assert!(NsPath::parse("a/b/c/d").is_err());
    }

    #[test]
    fn join_key_normalizes_slashes() {
        assert_eq!(join_key("a/b/", "/c"), "a/b/c");
        assert_eq!(join_key("a/b", "c"), "a/b/c");
    }

    #[test]
    fn idem_tokens_are_stable_and_hex() {
        let t1 = idem_token("client-batch-42");
        let t2 = idem_token("client-batch-42");
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 24);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t1, idem_token("client-batch-43"));
    }
}
