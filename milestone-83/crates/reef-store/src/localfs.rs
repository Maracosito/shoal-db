//! Local filesystem object store.
//!
//! Used for dev mode and the filesystem-backed test matrix. Durability and
//! atomicity strategy:
//!
//! - `put`: write to a temp file under `<root>/.reef-tmp/`, `fsync` the file,
//!   then `rename(2)` it into place (atomic on POSIX), then best-effort
//!   `fsync` the parent directory. Readers never observe partial objects.
//! - `put_if_absent`: write + fsync a temp file, then `link(2)` it to the
//!   target path. Hard-link creation fails with `EEXIST` if the target
//!   exists, which gives an atomic create-if-absent without races.
//!
//! All filesystem work runs on the blocking thread pool via
//! `tokio::task::spawn_blocking`.

use crate::{validate_key, validate_prefix, ObjectMeta, ObjectStore, StoreError, StoreResult};
use async_trait::async_trait;
use bytes::Bytes;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use ulid::Ulid;

const TMP_DIR: &str = ".reef-tmp";

/// Filesystem-backed object store rooted at a directory.
#[derive(Debug, Clone)]
pub struct LocalFsStore {
    root: PathBuf,
    tmp: PathBuf,
}

impl LocalFsStore {
    pub fn new(root: impl Into<PathBuf>) -> StoreResult<Self> {
        let root: PathBuf = root.into();
        let tmp = root.join(TMP_DIR);
        fs::create_dir_all(&tmp).map_err(|e| StoreError::Io {
            path: tmp.display().to_string(),
            source: e,
        })?;
        Ok(LocalFsStore { root, tmp })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn abs(&self, key: &str) -> StoreResult<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    fn write_tmp(&self, data: &[u8]) -> StoreResult<PathBuf> {
        let p = self.tmp.join(Ulid::new().to_string());
        let io = |e| StoreError::Io {
            path: p.display().to_string(),
            source: e,
        };
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .map_err(io)?;
        f.write_all(data).map_err(io)?;
        f.sync_all().map_err(io)?;
        Ok(p)
    }

    fn ensure_parent(target: &Path) -> StoreResult<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| StoreError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        Ok(())
    }

    fn fsync_dir(dir: Option<&Path>) {
        // Best-effort directory fsync so the rename itself is durable.
        #[cfg(unix)]
        if let Some(dir) = dir {
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
        }
        #[cfg(not(unix))]
        let _ = dir;
    }

    fn put_blocking(&self, key: &str, data: &[u8]) -> StoreResult<()> {
        let target = self.abs(key)?;
        Self::ensure_parent(&target)?;
        let tmp = self.write_tmp(data)?;
        fs::rename(&tmp, &target).map_err(|e| StoreError::Io {
            path: target.display().to_string(),
            source: e,
        })?;
        Self::fsync_dir(target.parent());
        Ok(())
    }

    fn put_if_absent_blocking(&self, key: &str, data: &[u8]) -> StoreResult<()> {
        let target = self.abs(key)?;
        Self::ensure_parent(&target)?;
        let tmp = self.write_tmp(data)?;
        let result = fs::hard_link(&tmp, &target);
        // The temp file is no longer needed whether the link succeeded or not.
        let _ = fs::remove_file(&tmp);
        match result {
            Ok(()) => {
                Self::fsync_dir(target.parent());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(StoreError::AlreadyExists(key.to_string()))
            }
            Err(e) => Err(StoreError::Io {
                path: target.display().to_string(),
                source: e,
            }),
        }
    }

    fn get_blocking(&self, key: &str) -> StoreResult<Bytes> {
        let path = self.abs(key)?;
        match fs::read(&path) {
            Ok(data) => Ok(Bytes::from(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(key.to_string()))
            }
            Err(e) => Err(StoreError::Io {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }

    fn get_range_blocking(&self, key: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        let path = self.abs(key)?;
        let io = |e: std::io::Error| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(key.to_string())
            } else {
                StoreError::Io {
                    path: path.display().to_string(),
                    source: e,
                }
            }
        };
        let mut f = File::open(&path).map_err(io)?;
        let size = f.metadata().map_err(io)?.len();
        if offset > size {
            return Err(StoreError::InvalidRange {
                path: key.to_string(),
                offset,
                len,
                size,
            });
        }
        let take = len.min(size - offset);
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = vec![0u8; take as usize];
        f.read_exact(&mut buf).map_err(io)?;
        Ok(Bytes::from(buf))
    }

    fn head_blocking(&self, key: &str) -> StoreResult<ObjectMeta> {
        let path = self.abs(key)?;
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(ObjectMeta {
                path: key.to_string(),
                size: meta.len(),
            }),
            Ok(_) => Err(StoreError::NotFound(key.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::NotFound(key.to_string()))
            }
            Err(e) => Err(StoreError::Io {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }

    fn list_blocking(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>> {
        validate_prefix(prefix)?;
        // Start from the deepest directory implied by the prefix.
        let dir_part = match prefix.rfind('/') {
            Some(i) => &prefix[..i],
            None => "",
        };
        let start = if dir_part.is_empty() {
            self.root.clone()
        } else {
            self.root.join(dir_part)
        };
        let mut out = Vec::new();
        if !start.is_dir() {
            return Ok(out);
        }
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(&dir).map_err(|e| StoreError::Io {
                path: dir.display().to_string(),
                source: e,
            })?;
            for entry in entries {
                let entry = entry.map_err(|e| StoreError::Io {
                    path: dir.display().to_string(),
                    source: e,
                })?;
                let path = entry.path();
                // Skip the internal temp directory.
                if path == self.tmp {
                    continue;
                }
                let meta = entry.metadata().map_err(|e| StoreError::Io {
                    path: path.display().to_string(),
                    source: e,
                })?;
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    let rel = path
                        .strip_prefix(&self.root)
                        .map_err(|_| StoreError::Backend("path outside root".to_string()))?;
                    let key: String = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    if key.starts_with(prefix) {
                        out.push(ObjectMeta {
                            path: key,
                            size: meta.len(),
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn delete_blocking(&self, key: &str) -> StoreResult<()> {
        let path = self.abs(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io {
                path: path.display().to_string(),
                source: e,
            }),
        }
    }
}

fn join_err(e: tokio::task::JoinError) -> StoreError {
    StoreError::Backend(format!("blocking task failed: {e}"))
}

#[async_trait]
impl ObjectStore for LocalFsStore {
    async fn put(&self, path: &str, data: Bytes) -> StoreResult<()> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.put_blocking(&key, &data))
            .await
            .map_err(join_err)?
    }

    async fn put_if_absent(&self, path: &str, data: Bytes) -> StoreResult<()> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.put_if_absent_blocking(&key, &data))
            .await
            .map_err(join_err)?
    }

    async fn get(&self, path: &str) -> StoreResult<Bytes> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.get_blocking(&key))
            .await
            .map_err(join_err)?
    }

    async fn get_range(&self, path: &str, offset: u64, len: u64) -> StoreResult<Bytes> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.get_range_blocking(&key, offset, len))
            .await
            .map_err(join_err)?
    }

    async fn head(&self, path: &str) -> StoreResult<ObjectMeta> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.head_blocking(&key))
            .await
            .map_err(join_err)?
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectMeta>> {
        let this = self.clone();
        let p = prefix.to_string();
        tokio::task::spawn_blocking(move || this.list_blocking(&p))
            .await
            .map_err(join_err)?
    }

    async fn delete(&self, path: &str) -> StoreResult<()> {
        let this = self.clone();
        let key = path.to_string();
        tokio::task::spawn_blocking(move || this.delete_blocking(&key))
            .await
            .map_err(join_err)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, LocalFsStore) {
        let dir = TempDir::new().unwrap();
        let store = LocalFsStore::new(dir.path()).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (_dir, store) = store();
        store
            .put("ns/abc/wal/000001.wal", Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert_eq!(
            store.get("ns/abc/wal/000001.wal").await.unwrap(),
            Bytes::from_static(b"data")
        );
        assert!(store.get("ns/missing").await.unwrap_err().is_not_found());
    }

    #[tokio::test]
    async fn put_overwrites() {
        let (_dir, store) = store();
        store.put("k", Bytes::from_static(b"v1")).await.unwrap();
        store.put("k", Bytes::from_static(b"v2")).await.unwrap();
        assert_eq!(store.get("k").await.unwrap(), Bytes::from_static(b"v2"));
    }

    #[tokio::test]
    async fn put_if_absent_is_atomic_create() {
        let (_dir, store) = store();
        store
            .put_if_absent("m/1", Bytes::from_static(b"a"))
            .await
            .unwrap();
        let err = store
            .put_if_absent("m/1", Bytes::from_static(b"b"))
            .await
            .unwrap_err();
        assert!(err.is_already_exists());
        assert_eq!(store.get("m/1").await.unwrap(), Bytes::from_static(b"a"));
    }

    #[tokio::test]
    async fn concurrent_put_if_absent_single_winner() {
        let (_dir, store) = store();
        let mut handles = Vec::new();
        for i in 0..16u32 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                s.put_if_absent("race", Bytes::from(i.to_string())).await
            }));
        }
        let mut wins = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "exactly one concurrent creator must win");
    }

    #[tokio::test]
    async fn get_range_and_head() {
        let (_dir, store) = store();
        store.put("k", Bytes::from_static(b"0123456789")).await.unwrap();
        assert_eq!(
            store.get_range("k", 3, 4).await.unwrap(),
            Bytes::from_static(b"3456")
        );
        assert_eq!(
            store.get_range("k", 8, 100).await.unwrap(),
            Bytes::from_static(b"89")
        );
        assert!(matches!(
            store.get_range("k", 20, 1).await,
            Err(StoreError::InvalidRange { .. })
        ));
        assert_eq!(store.head("k").await.unwrap().size, 10);
    }

    #[tokio::test]
    async fn list_recursive_and_sorted() {
        let (_dir, store) = store();
        for k in [
            "ns/a/wal/000002.wal",
            "ns/a/wal/000001.wal",
            "ns/a/segments/s1.seg",
            "ns/b/wal/000001.wal",
        ] {
            store.put(k, Bytes::from_static(b"x")).await.unwrap();
        }
        let listed = store.list("ns/a/wal/").await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(keys, vec!["ns/a/wal/000001.wal", "ns/a/wal/000002.wal"]);

        // Prefix that is not a directory boundary.
        let listed = store.list("ns/a/wal/000001").await.unwrap();
        assert_eq!(listed.len(), 1);

        // Everything under ns/a.
        assert_eq!(store.list("ns/a/").await.unwrap().len(), 3);

        // Temp dir is never listed.
        assert!(store
            .list("")
            .await
            .unwrap()
            .iter()
            .all(|m| !m.path.contains(".reef-tmp")));
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (_dir, store) = store();
        store.put("k", Bytes::from_static(b"x")).await.unwrap();
        store.delete("k").await.unwrap();
        store.delete("k").await.unwrap();
        assert!(!store.exists("k").await.unwrap());
    }

    #[tokio::test]
    async fn no_temp_files_leak() {
        let (dir, store) = store();
        for i in 0..10 {
            store
                .put(&format!("k/{i}"), Bytes::from_static(b"x"))
                .await
                .unwrap();
            let _ = store
                .put_if_absent("k/0", Bytes::from_static(b"y"))
                .await;
        }
        let tmp_entries: Vec<_> = std::fs::read_dir(dir.path().join(TMP_DIR))
            .unwrap()
            .collect();
        assert!(tmp_entries.is_empty(), "temp files leaked: {tmp_entries:?}");
    }

    #[tokio::test]
    async fn survives_reopen() {
        let dir = TempDir::new().unwrap();
        {
            let store = LocalFsStore::new(dir.path()).unwrap();
            store.put("a/b", Bytes::from_static(b"persist")).await.unwrap();
        }
        let store2 = LocalFsStore::new(dir.path()).unwrap();
        assert_eq!(
            store2.get("a/b").await.unwrap(),
            Bytes::from_static(b"persist")
        );
    }
}
