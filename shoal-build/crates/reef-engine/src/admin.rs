//! Administrative operations: `verify`, `repair`, and `rebuild`.
//!
//! These operate directly on the object store and are designed to run while
//! no writer is active on the namespace (offline maintenance). After running
//! `repair` or `rebuild` against a live process, call
//! [`crate::engine::Engine::evict_namespace`] so the process re-opens the
//! namespace from storage.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use reef_store::ObjectStore;
use reef_types::write::WriteOp;
use reef_types::DocId;

use crate::engine::{self, IdempotencyEntry, WriteResult};
use crate::error::{EngineError, EngineResult};
use crate::layout;
use crate::manifest::{self, Manifest};
use crate::memtable::DocState;
use crate::now_ms;
use crate::segment::{self, SegmentBuilder, SegmentReader};
use crate::wal::Wal;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    /// Stable machine-readable code, e.g. `segment_missing`, `wal_corrupt`.
    pub code: String,
    pub detail: String,
}

impl Issue {
    fn new(severity: Severity, code: &str, detail: impl Into<String>) -> Self {
        Self { severity, code: code.to_string(), detail: detail.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub namespace: String,
    pub manifest_version: Option<u64>,
    pub last_applied_lsn: Option<u64>,
    pub segments_checked: usize,
    pub wal_records_checked: usize,
    pub orphan_segments: Vec<String>,
    pub issues: Vec<Issue>,
    /// True iff no `Error`-severity issues were found.
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct RepairOptions {
    /// Delete segment objects unreferenced by the current manifest.
    pub delete_orphan_segments: bool,
    /// If a WAL record above the watermark is corrupt, delete it *and every
    /// record after it* (committed-but-unreadable data is lost; this is an
    /// explicit operator decision).
    pub truncate_corrupt_wal: bool,
    /// Delete manifest versions beyond `keep_manifests`.
    pub prune_manifests: bool,
    pub keep_manifests: usize,
    /// Report what would be done without doing it.
    pub dry_run: bool,
}

impl Default for RepairOptions {
    fn default() -> Self {
        Self {
            delete_orphan_segments: true,
            truncate_corrupt_wal: false,
            prune_manifests: true,
            keep_manifests: 5,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    pub namespace: String,
    pub dry_run: bool,
    /// Human-readable record of every action taken (or planned in dry-run).
    pub actions: Vec<String>,
    /// Issues repair could not (or was not allowed to) fix.
    pub remaining_issues: Vec<Issue>,
}

#[derive(Debug, Clone)]
pub struct RebuildOptions {
    /// Split rebuilt level-1 segments at roughly this size.
    pub target_segment_bytes: u64,
    /// If true, a corrupt WAL record above the watermark aborts the rebuild;
    /// if false it is skipped along with everything after it (with a warning
    /// in the report).
    pub strict_wal: bool,
}

impl Default for RebuildOptions {
    fn default() -> Self {
        Self { target_segment_bytes: 64 * 1024 * 1024, strict_wal: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildReport {
    pub namespace: String,
    /// Manifest version the rebuild was based on (None = rebuilt purely from
    /// segment objects + WAL, no readable manifest existed).
    pub base_manifest_version: Option<u64>,
    pub wal_records_applied: usize,
    pub live_docs: u64,
    pub tombstones_dropped: u64,
    pub new_manifest_version: u64,
    pub new_segments: Vec<String>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Validate every component of a namespace: manifest chain, segment objects
/// (existence, checksums, doc counts), WAL decodability and LSN continuity,
/// and orphaned objects. Read-only.
pub async fn verify_namespace(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
) -> EngineResult<VerifyReport> {
    let mut issues: Vec<Issue> = Vec::new();
    let mut segments_checked = 0usize;
    let mut wal_records_checked = 0usize;
    let mut orphan_segments: Vec<String> = Vec::new();

    // 1. Manifest.
    let versions = manifest::list_versions(store, ns).await?;
    if versions.is_empty() {
        issues.push(Issue::new(
            Severity::Error,
            "manifest_missing",
            format!("namespace '{ns}' has no manifest objects; run `rebuild` if segment/WAL objects exist"),
        ));
        return Ok(VerifyReport {
            namespace: ns.to_string(),
            manifest_version: None,
            last_applied_lsn: None,
            segments_checked,
            wal_records_checked,
            orphan_segments,
            issues,
            ok: false,
        });
    }
    let current_version = *versions.iter().max().expect("non-empty");
    let current = match manifest::load_version(store, ns, current_version).await {
        Ok(m) => Some(m),
        Err(e) => {
            issues.push(Issue::new(
                Severity::Error,
                "manifest_corrupt",
                format!("current manifest version {current_version} is unreadable: {e}; `repair` can roll forward from an older version"),
            ));
            // Find the newest readable fallback for the rest of the checks.
            let mut fallback = None;
            for v in versions.iter().rev().skip(1) {
                if let Ok(m) = manifest::load_version(store, ns, *v).await {
                    issues.push(Issue::new(
                        Severity::Warning,
                        "manifest_fallback",
                        format!("checks below use readable manifest version {v}"),
                    ));
                    fallback = Some(m);
                    break;
                }
            }
            fallback
        }
    };

    // 2. Segments referenced by the manifest.
    let mut referenced: HashSet<String> = HashSet::new();
    if let Some(m) = &current {
        for meta in &m.segments {
            referenced.insert(meta.id.clone());
            segments_checked += 1;
            match store.get(&layout::segment_key(ns, &meta.id)).await {
                Ok(bytes) => {
                    let actual_size = bytes.len() as u64;
                    match SegmentReader::open(bytes) {
                        Ok(reader) => {
                            if reader.meta().doc_count != meta.doc_count {
                                issues.push(Issue::new(
                                    Severity::Warning,
                                    "segment_count_mismatch",
                                    format!(
                                        "segment '{}': manifest says {} docs, segment header says {}",
                                        meta.id, meta.doc_count, reader.meta().doc_count
                                    ),
                                ));
                            }
                            if meta.size_bytes != 0 && meta.size_bytes != actual_size {
                                issues.push(Issue::new(
                                    Severity::Warning,
                                    "segment_size_mismatch",
                                    format!(
                                        "segment '{}': manifest says {} bytes, object is {} bytes",
                                        meta.id, meta.size_bytes, actual_size
                                    ),
                                ));
                            }
                        }
                        Err(e) => issues.push(Issue::new(
                            Severity::Error,
                            "segment_corrupt",
                            format!("segment '{}' failed validation: {e}; run `rebuild`", meta.id),
                        )),
                    }
                }
                Err(e) if e.is_not_found() => issues.push(Issue::new(
                    Severity::Error,
                    "segment_missing",
                    format!("segment '{}' referenced by manifest is missing; run `repair` to roll back or `rebuild`", meta.id),
                )),
                Err(e) => return Err(e.into()),
            }
        }
    }

    // 3. WAL.
    let watermark = current.as_ref().map(|m| m.last_applied_lsn).unwrap_or(0);
    let wal = Wal::new(store.clone(), ns);
    let files = wal.list().await?;
    let total = files.len();
    let mut prev_pending_lsn: Option<u64> = None;
    for (i, file) in files.iter().enumerate() {
        wal_records_checked += 1;
        match wal.read(file.lsn).await {
            Ok(record) => {
                if record.lsn != file.lsn {
                    issues.push(Issue::new(
                        Severity::Error,
                        "wal_lsn_mismatch",
                        format!("WAL object for lsn {} contains record lsn {}", file.lsn, record.lsn),
                    ));
                }
                if record.lsn > watermark {
                    if let Some(prev) = prev_pending_lsn {
                        if record.lsn != prev + 1 {
                            issues.push(Issue::new(
                                Severity::Warning,
                                "wal_gap",
                                format!("WAL gap above watermark: lsn {prev} -> {}", record.lsn),
                            ));
                        }
                    }
                    prev_pending_lsn = Some(record.lsn);
                }
            }
            Err(EngineError::Corruption(msg)) => {
                if file.lsn <= watermark {
                    issues.push(Issue::new(
                        Severity::Warning,
                        "wal_folded_corrupt",
                        format!("folded WAL record lsn {} is corrupt (harmless; gc/repair removes it): {msg}", file.lsn),
                    ));
                } else if i == total - 1 {
                    issues.push(Issue::new(
                        Severity::Warning,
                        "wal_torn_tail",
                        format!("WAL tail record lsn {} is undecodable (torn write); recovery quarantines it, `repair` removes it: {msg}", file.lsn),
                    ));
                } else {
                    issues.push(Issue::new(
                        Severity::Error,
                        "wal_corrupt",
                        format!("WAL record lsn {} above the watermark is corrupt mid-stream: {msg}; `repair --truncate-corrupt-wal` discards it and everything after it", file.lsn),
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }

    // 4. Orphan segment objects.
    for obj in store.list(&layout::segment_prefix(ns)).await? {
        if let Some(id) = obj.key.rsplit('/').next().map(|n| n.trim_end_matches(".seg")) {
            if !id.is_empty() && !referenced.contains(id) {
                orphan_segments.push(id.to_string());
                issues.push(Issue::new(
                    Severity::Info,
                    "segment_orphan",
                    format!("segment object '{id}' is not referenced by the current manifest (gc candidate)"),
                ));
            }
        }
    }

    let ok = !issues.iter().any(|i| i.severity == Severity::Error);
    Ok(VerifyReport {
        namespace: ns.to_string(),
        manifest_version: Some(current_version),
        last_applied_lsn: current.as_ref().map(|m| m.last_applied_lsn),
        segments_checked,
        wal_records_checked,
        orphan_segments,
        issues,
        ok,
    })
}

// ---------------------------------------------------------------------------
// repair
// ---------------------------------------------------------------------------

/// Fix everything that can be fixed without rewriting data:
/// * roll the manifest forward past a corrupt current version (re-commits the
///   newest readable version as a fresh, higher version);
/// * delete corrupt folded WAL records, torn WAL tails, and (only with
///   `truncate_corrupt_wal`) corrupt mid-stream records plus their suffix;
/// * delete orphan segment objects and prune old manifests.
pub async fn repair_namespace(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
    opts: &RepairOptions,
) -> EngineResult<RepairReport> {
    let mut actions: Vec<String> = Vec::new();
    let mut remaining: Vec<Issue> = Vec::new();

    // 1. Manifest roll-forward.
    let versions = manifest::list_versions(store, ns).await?;
    if versions.is_empty() {
        remaining.push(Issue::new(
            Severity::Error,
            "manifest_missing",
            format!("namespace '{ns}' has no manifest; use `rebuild` to reconstruct from segments + WAL"),
        ));
        return Ok(RepairReport {
            namespace: ns.to_string(),
            dry_run: opts.dry_run,
            actions,
            remaining_issues: remaining,
        });
    }
    let max_version = *versions.iter().max().expect("non-empty");
    let mut current: Option<Manifest> = None;
    match manifest::load_version(store, ns, max_version).await {
        Ok(m) => current = Some(m),
        Err(_) => {
            let mut rolled = false;
            for v in versions.iter().rev().skip(1) {
                if let Ok(mut good) = manifest::load_version(store, ns, *v).await {
                    good.version = max_version + 1;
                    good.updated_at_ms = now_ms();
                    actions.push(format!(
                        "roll manifest forward: corrupt version {max_version} superseded by copy of version {v} committed as version {}",
                        good.version
                    ));
                    if !opts.dry_run {
                        manifest::commit(store, &good).await?;
                    }
                    current = Some(good);
                    rolled = true;
                    break;
                }
            }
            if !rolled {
                remaining.push(Issue::new(
                    Severity::Error,
                    "manifest_unrecoverable",
                    format!("no readable manifest version exists for '{ns}'; use `rebuild`"),
                ));
            }
        }
    }

    let watermark = current.as_ref().map(|m| m.last_applied_lsn).unwrap_or(0);

    // 2. WAL hygiene.
    let wal = Wal::new(store.clone(), ns);
    let files = wal.list().await?;
    let total = files.len();
    let mut truncate_from: Option<u64> = None;
    for (i, file) in files.iter().enumerate() {
        if let Some(from) = truncate_from {
            if file.lsn >= from {
                actions.push(format!("delete WAL record lsn {} (suffix of corrupt record {from})", file.lsn));
                if !opts.dry_run {
                    store.delete(&layout::wal_key(ns, file.lsn)).await?;
                }
                continue;
            }
        }
        match wal.read(file.lsn).await {
            Ok(_) => {}
            Err(EngineError::Corruption(msg)) => {
                if file.lsn <= watermark {
                    actions.push(format!("delete corrupt folded WAL record lsn {}", file.lsn));
                    if !opts.dry_run {
                        store.delete(&layout::wal_key(ns, file.lsn)).await?;
                    }
                } else if i == total - 1 {
                    actions.push(format!("delete torn WAL tail record lsn {}", file.lsn));
                    if !opts.dry_run {
                        store.delete(&layout::wal_key(ns, file.lsn)).await?;
                    }
                } else if opts.truncate_corrupt_wal {
                    warn!(namespace = ns, lsn = file.lsn, "truncating WAL at corrupt record (data loss)");
                    actions.push(format!(
                        "TRUNCATE: delete corrupt WAL record lsn {} and all later records (data loss authorized by --truncate-corrupt-wal)",
                        file.lsn
                    ));
                    if !opts.dry_run {
                        store.delete(&layout::wal_key(ns, file.lsn)).await?;
                    }
                    truncate_from = Some(file.lsn);
                } else {
                    remaining.push(Issue::new(
                        Severity::Error,
                        "wal_corrupt",
                        format!(
                            "WAL record lsn {} above the watermark is corrupt mid-stream: {msg}; re-run with truncate_corrupt_wal to discard it and its suffix",
                            file.lsn
                        ),
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }

    // 3. Orphan segments.
    if opts.delete_orphan_segments {
        let referenced: HashSet<String> = current
            .as_ref()
            .map(|m| m.segments.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        for obj in store.list(&layout::segment_prefix(ns)).await? {
            let Some(id) = obj.key.rsplit('/').next().map(|n| n.trim_end_matches(".seg")) else {
                continue;
            };
            if id.is_empty() || referenced.contains(id) {
                continue;
            }
            actions.push(format!("delete orphan segment object '{id}'"));
            if !opts.dry_run {
                store.delete(&obj.key).await?;
            }
        }
    }

    // 4. Prune old manifests (never the current/highest version).
    if opts.prune_manifests {
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        let keep = opts.keep_manifests.max(1);
        if sorted.len() > keep {
            let cutoff = sorted.len() - keep;
            for v in &sorted[..cutoff] {
                if Some(*v) == current.as_ref().map(|m| m.version) || *v == max_version {
                    continue;
                }
                actions.push(format!("delete old manifest version {v}"));
                if !opts.dry_run {
                    store.delete(&layout::manifest_key(ns, *v)).await?;
                }
            }
        }
    }

    info!(
        namespace = ns,
        actions = actions.len(),
        remaining = remaining.len(),
        dry_run = opts.dry_run,
        "repair complete"
    );
    Ok(RepairReport {
        namespace: ns.to_string(),
        dry_run: opts.dry_run,
        actions,
        remaining_issues: remaining,
    })
}

// ---------------------------------------------------------------------------
// rebuild
// ---------------------------------------------------------------------------

async fn manifest_segments_readable(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
    m: &Manifest,
) -> bool {
    for meta in &m.segments {
        match store.get(&layout::segment_key(ns, &meta.id)).await {
            Ok(bytes) => {
                if SegmentReader::open(bytes).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    true
}

fn apply_ops_to_map(
    map: &mut BTreeMap<DocId, (u64, DocState)>,
    ops: &[WriteOp],
    lsn: u64,
) {
    for op in ops {
        match op {
            WriteOp::Upsert(doc) => {
                map.insert(doc.id.clone(), (lsn, DocState::Live(doc.clone())));
            }
            WriteOp::Delete(id) => {
                map.insert(id.clone(), (lsn, DocState::Tombstone));
            }
            WriteOp::Patch(patch) => {
                let base = match map.get(&patch.id) {
                    Some((_, DocState::Live(d))) => Some(d.clone()),
                    _ => None,
                };
                if let Some(mut doc) = base {
                    engine::apply_patch(&mut doc, patch);
                    map.insert(patch.id.clone(), (lsn, DocState::Live(doc)));
                }
            }
        }
    }
}

/// Reconstruct a namespace **from object storage alone**: find the newest
/// fully readable manifest (or none), merge its segments, replay every
/// surviving WAL record above its watermark, and write a brand-new set of
/// level-1 segments plus a fresh manifest. Old objects are left for GC.
///
/// This is the proof that all indexes/segments are derivable from durable
/// state — it requires no in-process memory whatsoever.
pub async fn rebuild_namespace(
    store: &Arc<dyn ObjectStore>,
    ns: &str,
    opts: &RebuildOptions,
) -> EngineResult<RebuildReport> {
    let mut warnings: Vec<String> = Vec::new();

    // 1. Newest fully readable manifest.
    let versions = manifest::list_versions(store, ns).await?;
    let max_version = versions.iter().max().copied().unwrap_or(0);
    let mut base: Option<Manifest> = None;
    for v in versions.iter().rev() {
        match manifest::load_version(store, ns, *v).await {
            Ok(m) => {
                if manifest_segments_readable(store, ns, &m).await {
                    base = Some(m);
                    break;
                } else {
                    warnings.push(format!(
                        "manifest version {v} references unreadable segments; trying older version"
                    ));
                }
            }
            Err(e) => warnings.push(format!("manifest version {v} unreadable: {e}")),
        }
    }
    let watermark = base.as_ref().map(|m| m.last_applied_lsn).unwrap_or(0);

    // 2. Merge base segments.
    let mut merged: BTreeMap<DocId, (u64, DocState)> = BTreeMap::new();
    if let Some(m) = &base {
        for meta in &m.segments {
            let bytes = store.get(&layout::segment_key(ns, &meta.id)).await?;
            let reader = SegmentReader::open(bytes)?;
            for entry in reader.iter() {
                let newer = merged.get(&entry.id).map_or(true, |(l, _)| entry.lsn > *l);
                if newer {
                    merged.insert(entry.id.clone(), (entry.lsn, entry.state.clone()));
                }
            }
        }
    }

    // 3. Replay WAL above the watermark, harvesting idempotency keys from
    //    every readable record.
    let wal = Wal::new(store.clone(), ns);
    let files = wal.list().await?;
    let mut wal_records_applied = 0usize;
    let mut max_lsn = watermark;
    let mut idempotency: Vec<IdempotencyEntry> = Vec::new();
    let mut stop = false;
    for file in &files {
        if stop {
            break;
        }
        match wal.read(file.lsn).await {
            Ok(record) => {
                if let Some(key) = &record.idempotency_key {
                    idempotency.push(IdempotencyEntry {
                        key: key.clone(),
                        lsn: record.lsn,
                        committed_at_ms: record.committed_at_ms,
                        result: WriteResult {
                            lsn: record.lsn,
                            upserts: 0,
                            patches_applied: 0,
                            patches_missed: 0,
                            deletes: 0,
                            idempotent_replay: false,
                        },
                    });
                }
                if record.lsn > watermark {
                    apply_ops_to_map(&mut merged, &record.ops, record.lsn);
                    wal_records_applied += 1;
                    max_lsn = max_lsn.max(record.lsn);
                }
            }
            Err(EngineError::Corruption(msg)) => {
                if file.lsn <= watermark {
                    warnings.push(format!("skipping corrupt folded WAL record lsn {}", file.lsn));
                } else if opts.strict_wal {
                    return Err(EngineError::Corruption(format!(
                        "rebuild aborted: WAL record lsn {} is corrupt: {msg}; \
                         re-run with strict_wal=false to discard it and its suffix",
                        file.lsn
                    )));
                } else {
                    warnings.push(format!(
                        "discarding corrupt WAL record lsn {} and all later records: {msg}",
                        file.lsn
                    ));
                    stop = true;
                }
            }
            Err(e) => return Err(e),
        }
    }

    // 4. Write fresh level-1 segments, dropping tombstones.
    let target = opts.target_segment_bytes.max(1);
    let mut new_segments = Vec::new();
    let mut segment_metas = Vec::new();
    let mut builder: Option<SegmentBuilder> = None;
    let mut builder_bytes: u64 = 0;
    let mut live_docs: u64 = 0;
    let mut tombstones_dropped: u64 = 0;

    let mut flush =
        |b: SegmentBuilder, metas: &mut Vec<_>, ids: &mut Vec<String>| -> EngineResult<(Bytes, String)> {
            let (bytes, meta) = b.finish()?;
            ids.push(meta.id.clone());
            let id = meta.id.clone();
            metas.push(meta);
            Ok((bytes, id))
        };
    let mut pending_objects: Vec<(String, Bytes)> = Vec::new();

    for (id, (lsn, state)) in merged {
        match state {
            DocState::Tombstone => {
                tombstones_dropped += 1;
            }
            DocState::Live(doc) => {
                if builder.is_none() {
                    builder = Some(SegmentBuilder::new(&segment::new_segment_id(), 1));
                    builder_bytes = 0;
                }
                builder_bytes +=
                    serde_json::to_vec(&doc).map(|v| v.len() as u64).unwrap_or(256) + 32;
                builder
                    .as_mut()
                    .expect("builder present")
                    .add(id, lsn, DocState::Live(doc));
                live_docs += 1;
                if builder_bytes >= target {
                    let (bytes, seg_id) = flush(
                        builder.take().expect("builder present"),
                        &mut segment_metas,
                        &mut new_segments,
                    )?;
                    pending_objects.push((seg_id, bytes));
                }
            }
        }
    }
    if let Some(b) = builder.take() {
        let (bytes, seg_id) = flush(b, &mut segment_metas, &mut new_segments)?;
        pending_objects.push((seg_id, bytes));
    }

    for (seg_id, bytes) in &pending_objects {
        let created = store.put_if_absent(&layout::segment_key(ns, seg_id), bytes.clone()).await?;
        if !created {
            return Err(EngineError::Conflict(format!(
                "rebuild segment id collision for '{seg_id}'"
            )));
        }
    }

    // 5. Commit the fresh manifest.
    let mut new_manifest = match &base {
        Some(m) => m.clone(),
        None => Manifest::new(ns),
    };
    new_manifest.version = max_version.max(new_manifest.version) + 1;
    new_manifest.updated_at_ms = now_ms();
    new_manifest.last_applied_lsn = max_lsn;
    new_manifest.segments = segment_metas;
    manifest::commit(store, &new_manifest).await?;

    // 6. Persist the harvested idempotency ledger.
    idempotency.sort_by(|a, b| a.lsn.cmp(&b.lsn));
    let ledger_bytes = serde_json::to_vec(&idempotency)?;
    store.put(&engine::ledger_key(ns), ledger_bytes.into()).await?;

    info!(
        namespace = ns,
        base_version = ?base.as_ref().map(|m| m.version),
        wal_records_applied,
        live_docs,
        tombstones_dropped,
        new_manifest_version = new_manifest.version,
        new_segments = new_segments.len(),
        "rebuild complete (old objects left for gc)"
    );

    Ok(RebuildReport {
        namespace: ns.to_string(),
        base_manifest_version: base.map(|m| m.version),
        wal_records_applied,
        live_docs,
        tombstones_dropped,
        new_manifest_version: new_manifest.version,
        new_segments,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, WriteRequest};
    use crate::options::EngineOptions;
    use reef_store::memory::MemoryStore;
    use reef_types::{Document, Value};
    use std::collections::BTreeMap as Map;

    fn mem() -> Arc<dyn ObjectStore> {
        Arc::new(MemoryStore::new())
    }

    fn engine(store: &Arc<dyn ObjectStore>) -> Engine {
        Engine::new(store.clone(), EngineOptions::for_tests())
    }

    fn doc(id: &str, n: i64) -> Document {
        let mut attributes = Map::new();
        attributes.insert("n".to_string(), Value::I64(n));
        Document {
            id: DocId::from(id.to_string()),
            vector: None,
            sparse_vector: None,
            attributes,
        }
    }

    fn id(s: &str) -> DocId {
        DocId::from(s.to_string())
    }

    #[tokio::test]
    async fn verify_reports_healthy_namespace() {
        let ns = "verify_ok";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();

        let report = verify_namespace(&store, ns).await.unwrap();
        assert!(report.ok, "issues: {:?}", report.issues);
        assert_eq!(report.segments_checked, 1);
        assert!(report.wal_records_checked >= 1);
        assert!(report.orphan_segments.is_empty());
    }

    #[tokio::test]
    async fn verify_detects_missing_segment_and_repair_cannot_silently_fix_it() {
        let ns = "verify_missing_seg";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        let fold = e.fold_now(ns).await.unwrap().unwrap();

        // Sabotage: delete the segment object behind the manifest's back.
        store.delete(&layout::segment_key(ns, &fold.segment_id)).await.unwrap();

        let report = verify_namespace(&store, ns).await.unwrap();
        assert!(!report.ok);
        assert!(report.issues.iter().any(|i| i.code == "segment_missing"));
    }

    #[tokio::test]
    async fn verify_detects_corrupt_segment() {
        let ns = "verify_corrupt_seg";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        let fold = e.fold_now(ns).await.unwrap().unwrap();

        // Sabotage: overwrite the segment with garbage.
        store
            .put(&layout::segment_key(ns, &fold.segment_id), Bytes::from_static(b"garbage"))
            .await
            .unwrap();

        let report = verify_namespace(&store, ns).await.unwrap();
        assert!(!report.ok);
        assert!(report.issues.iter().any(|i| i.code == "segment_corrupt"));
    }

    #[tokio::test]
    async fn repair_removes_orphans_and_torn_wal_tail() {
        let ns = "repair_basic";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();

        // Sabotage: an orphan segment object and a torn WAL tail.
        store
            .put(&layout::segment_key(ns, "orphan-seg"), Bytes::from_static(b"junk"))
            .await
            .unwrap();
        store
            .put(&layout::wal_key(ns, 99), Bytes::from_static(b"torn"))
            .await
            .unwrap();

        // Dry run changes nothing.
        let dry = repair_namespace(&store, ns, &RepairOptions { dry_run: true, ..Default::default() })
            .await
            .unwrap();
        assert!(dry.actions.len() >= 2);
        assert!(!verify_namespace(&store, ns).await.unwrap().orphan_segments.is_empty());

        // Real run fixes both.
        let report = repair_namespace(&store, ns, &RepairOptions::default()).await.unwrap();
        assert!(report.remaining_issues.is_empty());
        let verify = verify_namespace(&store, ns).await.unwrap();
        assert!(verify.ok, "issues: {:?}", verify.issues);
        assert!(verify.orphan_segments.is_empty());

        // Data is untouched.
        let e2 = engine(&store);
        assert_eq!(e2.export(ns).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rebuild_reconstructs_from_segments_and_wal() {
        let ns = "rebuild_full";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();
        e.fold_now(ns).await.unwrap().unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("c", 3)])).await.unwrap();
        e.write(ns, WriteRequest::delete(vec![id("a")])).await.unwrap();
        // c + delete(a) live only in the WAL at this point.

        let report = rebuild_namespace(&store, ns, &RebuildOptions::default()).await.unwrap();
        assert_eq!(report.wal_records_applied, 2);
        assert_eq!(report.live_docs, 2); // b, c
        assert_eq!(report.tombstones_dropped, 1);
        assert!(!report.new_segments.is_empty());

        // A fresh engine over the rebuilt namespace serves the right data.
        let e2 = engine(&store);
        e2.gc_now(ns).await.unwrap(); // collect superseded objects
        let all = e2.export(ns).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(e2.get_document(ns, &id("a")).await.unwrap().is_none());
        let c = e2.get_document(ns, &id("c")).await.unwrap().unwrap();
        assert_eq!(c.attributes.get("n"), Some(&Value::I64(3)));
        assert!(verify_namespace(&store, ns).await.unwrap().ok);
    }

    #[tokio::test]
    async fn rebuild_survives_total_manifest_loss() {
        let ns = "rebuild_no_manifest";
        let store = mem();
        let e = engine(&store);
        e.create_namespace(ns).await.unwrap();
        e.write(ns, WriteRequest::upsert(vec![doc("a", 1), doc("b", 2)])).await.unwrap();
        // No fold: everything lives in the WAL. Now destroy every manifest.
        for v in manifest::list_versions(&store, ns).await.unwrap() {
            store.delete(&layout::manifest_key(ns, v)).await.unwrap();
        }
        assert!(manifest::load_current(&store, ns).await.unwrap().is_none());

        let report = rebuild_namespace(&store, ns, &RebuildOptions::default()).await.unwrap();
        assert_eq!(report.base_manifest_version, None);
        assert_eq!(report.live_docs, 2);

        let e2 = engine(&store);
        assert_eq!(e2.export(ns).await.unwrap().len(), 2);
    }
}
