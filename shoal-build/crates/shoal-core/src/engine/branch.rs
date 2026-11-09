//! Copy-on-write namespace branching, namespace copies, and export.
//!
//! Branching creates a new namespace whose manifest references the *same*
//! immutable segment and WAL objects as the source — no data is copied, so
//! branching a multi-gigabyte namespace is a metadata-only operation.
//!
//! Isolation falls out of immutability:
//! - Writes to the branch append new WAL objects under the *branch's* prefix
//!   and commit to the *branch's* manifest; the source never sees them.
//! - Writes to the source likewise touch only the source manifest. The
//!   branch keeps referencing the exact object set captured at branch time.
//! - Compaction on either side replaces references in *that* manifest only;
//!   the refcount table keeps shared objects alive for the other side.
//! - Deleting either namespace decrements refcounts; objects are physically
//!   removed only at refcount zero.
//!
//! Multi-level branches (branch of a branch) need no special handling: a
//! branch manifest is a plain manifest, so branching it again just adds one
//! more reference per shared object.

use bytes::Bytes;

use crate::manifest::{Manifest, SegmentRef, WalRef};

use super::{join_key, now_ms, storage_err, Engine, EngineError, NamespaceInfo, NsPath, Result};

impl Engine {
    /// Create a copy-on-write branch of `src` at `dst`. O(metadata), not O(data).
    pub async fn branch_namespace(&self, src: &NsPath, dst: &NsPath) -> Result<NamespaceInfo> {
        self.fork(src, dst, true).await
    }

    /// Copy a namespace. `deep == false` performs a copy-on-write copy
    /// (identical mechanics to a branch, but without recording lineage);
    /// `deep == true` physically duplicates every referenced object so the
    /// copy shares nothing with the source.
    pub async fn copy_namespace(
        &self,
        src: &NsPath,
        dst: &NsPath,
        deep: bool,
    ) -> Result<NamespaceInfo> {
        if deep {
            self.deep_copy(src, dst).await
        } else {
            self.fork(src, dst, false).await
        }
    }

    async fn check_same_project(src: &NsPath, dst: &NsPath) -> Result<()> {
        if src.org != dst.org || src.project != dst.project {
            return Err(EngineError::Invalid(
                "branch/copy targets must be in the same organization and project \
                 (refcounts are tracked per project)"
                    .into(),
            ));
        }
        if src.root() == dst.root() {
            return Err(EngineError::Invalid(
                "source and destination namespaces must differ".into(),
            ));
        }
        Ok(())
    }

    async fn fork(&self, src: &NsPath, dst: &NsPath, as_branch: bool) -> Result<NamespaceInfo> {
        Self::check_same_project(src, dst).await?;

        // Deadlock-free two-namespace locking: always acquire in key order.
        let (first, second) = ordered(src.root(), dst.root());
        let g1 = self.ns_lock(&first).await.lock_owned().await;
        let g2 = self.ns_lock(&second).await.lock_owned().await;
        let _hold = (g1, g2);

        let src_manifest = self.load_manifest_from_store(src).await?;
        if self.namespace_exists(dst).await? {
            return Err(EngineError::NamespaceExists(dst.display()));
        }

        // Leak-only ordering: bump refcounts BEFORE the new manifest exists.
        let keys = src_manifest.object_keys();
        self.refs.incr(&dst.org, &dst.project, &keys).await?;

        let mut manifest = Manifest::new(&dst.display(), src_manifest.distance_metric.clone());
        manifest.parent = as_branch.then(|| src.display());
        manifest.labels = src_manifest.labels.clone();
        manifest.segments = src_manifest.segments.clone();
        manifest.wal = src_manifest.wal.clone();
        manifest.next_seq = src_manifest.next_seq;
        manifest.doc_count = src_manifest.doc_count;
        manifest.vector_dim = src_manifest.vector_dim;

        let committed = self.commit_manifest(dst, manifest).await?;
        Ok(self.info(dst, &committed))
    }

    async fn deep_copy(&self, src: &NsPath, dst: &NsPath) -> Result<NamespaceInfo> {
        Self::check_same_project(src, dst).await?;

        let (first, second) = ordered(src.root(), dst.root());
        let g1 = self.ns_lock(&first).await.lock_owned().await;
        let g2 = self.ns_lock(&second).await.lock_owned().await;
        let _hold = (g1, g2);

        let src_manifest = self.load_manifest_from_store(src).await?;
        if self.namespace_exists(dst).await? {
            return Err(EngineError::NamespaceExists(dst.display()));
        }

        let dst_root = dst.root();
        let mut new_segments = Vec::with_capacity(src_manifest.segments.len());
        let mut new_wal = Vec::with_capacity(src_manifest.wal.len());
        let mut new_keys = Vec::new();

        for seg in &src_manifest.segments {
            let bytes = self.cache.get(&seg.key).await.map_err(storage_err)?;
            let id = uuid::Uuid::new_v4().simple().to_string();
            let key = join_key(&dst_root, &format!("segments/{id}.seg"));
            self.store
                .put(&key, Bytes::from(bytes.to_vec()))
                .await
                .map_err(storage_err)?;
            new_keys.push(key.clone());
            new_segments.push(SegmentRef {
                id,
                key,
                doc_count: seg.doc_count,
                size_bytes: seg.size_bytes,
                created_at_ms: now_ms(),
            });
        }
        for wal in &src_manifest.wal {
            let bytes = self.cache.get(&wal.key).await.map_err(storage_err)?;
            let token = uuid::Uuid::new_v4().simple().to_string();
            let key = join_key(
                &dst_root,
                &format!("wal/{:020}-{token}.wal", wal.first_seq),
            );
            self.store
                .put(&key, Bytes::from(bytes.to_vec()))
                .await
                .map_err(storage_err)?;
            new_keys.push(key.clone());
            new_wal.push(WalRef {
                key,
                first_seq: wal.first_seq,
                last_seq: wal.last_seq,
                record_count: wal.record_count,
                size_bytes: wal.size_bytes,
            });
        }

        self.refs.incr(&dst.org, &dst.project, &new_keys).await?;

        let mut manifest = Manifest::new(&dst.display(), src_manifest.distance_metric.clone());
        manifest.labels = src_manifest.labels.clone();
        manifest.segments = new_segments;
        manifest.wal = new_wal;
        manifest.next_seq = src_manifest.next_seq;
        manifest.doc_count = src_manifest.doc_count;
        manifest.vector_dim = src_manifest.vector_dim;

        let committed = self.commit_manifest(dst, manifest).await?;
        Ok(self.info(dst, &committed))
    }

    /// Snapshot of all live documents, for export. The returned state is a
    /// consistent view at one manifest version; the HTTP layer streams it as
    /// NDJSON.
    pub async fn export_state(
        &self,
        path: &NsPath,
    ) -> Result<std::sync::Arc<super::state::NamespaceState>> {
        self.namespace_state(path).await
    }
}

fn ordered(a: String, b: String) -> (String, String) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}
