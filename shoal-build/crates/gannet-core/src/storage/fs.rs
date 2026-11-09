//! Local-filesystem [`ObjectStore`] backend.
//!
//! Used for development and single-machine deployments without an object
//! store, and as the storage backend for fast integration tests.
//!
//! Atomicity is achieved with the classic temp-file-then-rename pattern:
//! every write lands in a `.tmp` directory inside the store root (same
//! filesystem, so rename is atomic) and is `fsync`ed before being renamed
//! into place. `put_if_not_exists` uses a no-clobber rename (hard-link based
//! on Unix), which is atomic with respect to concurrent creators.
//!
//! All blocking filesystem work runs on the Tokio blocking pool.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{StorageError, StorageResult};

use super::{validate_key, validate_prefix, ObjectMeta, ObjectStore};

const TMP_DIR: &str = ".tmp";

/// An [`ObjectStore`] rooted at a local directory.
#[derive(Debug, Clone)]
pub struct FilesystemStore {
    root: Arc<PathBuf>,
}

impl FilesystemStore {
    /// Open (creating if necessary) a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> StorageResult<Self> {
        let root: PathBuf = root.into();
        fs::create_dir_all(&root).map_err(|e| io_err("<root>", e))?;
        fs::create_dir_all(root.join(TMP_DIR)).map_err(|e| io_err("<tmp>", e))?;
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// The root directory of this store.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn io_err(key: &str, e: std::io::Error) -> StorageError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound { key: key.into() }
    } else {
        StorageError::Io {
            key: key.into(),
            source: e,
        }
    }
}

async fn blocking<T, F>(f: F) -> StorageResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> StorageResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StorageError::backend("", format!("blocking task failed: {e}")))?
}

/// Best-effort fsync of the directory containing `path`, so the rename itself
/// is durable. Directory fsync is a Unix concept; on other platforms this is
/// a no-op.
fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn write_temp(root: &Path, key: &str, data: &[u8]) -> StorageResult<tempfile::NamedTempFile> {
    let tmp_dir = root.join(TMP_DIR);
    fs::create_dir_all(&tmp_dir).map_err(|e| io_err(key, e))?;
    let mut tmp = tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(key, e))?;
    tmp.write_all(data).map_err(|e| io_err(key, e))?;
    tmp.as_file().sync_all().map_err(|e| io_err(key, e))?;
    Ok(tmp)
}

fn dest_path(root: &Path, key: &str) -> StorageResult<PathBuf> {
    let dst = root.join(key);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(key, e))?;
    }
    Ok(dst)
}

fn put_sync(root: &Path, key: &str, data: &[u8], overwrite: bool) -> StorageResult<bool> {
    let dst = dest_path(root, key)?;
    let tmp = write_temp(root, key, data)?;
    if overwrite {
        tmp.persist(&dst).map_err(|e| io_err(key, e.error))?;
    } else {
        match tmp.persist_noclobber(&dst) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(io_err(key, e.error)),
        }
    }
    sync_parent_dir(&dst);
    Ok(true)
}

fn get_sync(root: &Path, key: &str) -> StorageResult<Bytes> {
    let data = fs::read(root.join(key)).map_err(|e| io_err(key, e))?;
    Ok(Bytes::from(data))
}

fn get_range_sync(root: &Path, key: &str, range: Range<u64>) -> StorageResult<Bytes> {
    let mut file = fs::File::open(root.join(key)).map_err(|e| io_err(key, e))?;
    let len = file.metadata().map_err(|e| io_err(key, e))?.len();
    if range.start >= len {
        return Err(StorageError::backend(
            key,
            format!("range start {} is at or past object size {len}", range.start),
        ));
    }
    let end = range.end.min(len);
    let to_read = (end - range.start) as usize;
    file.seek(SeekFrom::Start(range.start))
        .map_err(|e| io_err(key, e))?;
    let mut buf = vec![0u8; to_read];
    file.read_exact(&mut buf).map_err(|e| io_err(key, e))?;
    Ok(Bytes::from(buf))
}

fn head_sync(root: &Path, key: &str) -> StorageResult<ObjectMeta> {
    let md = fs::metadata(root.join(key)).map_err(|e| io_err(key, e))?;
    if !md.is_file() {
        return Err(StorageError::NotFound { key: key.into() });
    }
    Ok(ObjectMeta {
        key: key.to_string(),
        size: md.len(),
        last_modified: md.modified().ok(),
        etag: None,
    })
}

fn delete_sync(root: &Path, key: &str) -> StorageResult<()> {
    match fs::remove_file(root.join(key)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // idempotent
        Err(e) => Err(io_err(key, e)),
    }
}

fn list_sync(root: &Path, prefix: &str) -> StorageResult<Vec<ObjectMeta>> {
    // Start the walk at the deepest directory implied by the prefix so we do
    // not scan unrelated subtrees.
    let dir_part = match prefix.rfind('/') {
        Some(i) => &prefix[..=i],
        None => "",
    };
    let start = if dir_part.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_part)
    };
    let mut out = Vec::new();
    if !start.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| io_err(prefix, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_err(prefix, e))?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue; // backend-internal (e.g. the .tmp directory)
            }
            let file_type = entry.file_type().map_err(|e| io_err(prefix, e))?;
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .expect("walked path is under root");
                let key = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                if key.starts_with(prefix) {
                    let md = entry.metadata().map_err(|e| io_err(&key, e))?;
                    out.push(ObjectMeta {
                        key,
                        size: md.len(),
                        last_modified: md.modified().ok(),
                        etag: None,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

fn copy_sync(root: &Path, src: &str, dst: &str) -> StorageResult<()> {
    let src_path = root.join(src);
    let mut reader = fs::File::open(&src_path).map_err(|e| io_err(src, e))?;
    let dst_path = dest_path(root, dst)?;
    let tmp_dir = root.join(TMP_DIR);
    let mut tmp = tempfile::NamedTempFile::new_in(&tmp_dir).map_err(|e| io_err(dst, e))?;
    std::io::copy(&mut reader, tmp.as_file_mut()).map_err(|e| io_err(dst, e))?;
    tmp.as_file().sync_all().map_err(|e| io_err(dst, e))?;
    tmp.persist(&dst_path).map_err(|e| io_err(dst, e.error))?;
    sync_parent_dir(&dst_path);
    Ok(())
}

#[async_trait]
impl ObjectStore for FilesystemStore {
    async fn put(&self, key: &str, data: Bytes) -> StorageResult<()> {
        validate_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || put_sync(&root, &key, &data, true).map(|_| ())).await
    }

    async fn put_if_not_exists(&self, key: &str, data: Bytes) -> StorageResult<bool> {
        validate_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || put_sync(&root, &key, &data, false)).await
    }

    async fn get(&self, key: &str) -> StorageResult<Bytes> {
        validate_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || get_sync(&root, &key)).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> StorageResult<Bytes> {
        validate_key(key)?;
        if range.start >= range.end {
            return Ok(Bytes::new());
        }
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || get_range_sync(&root, &key, range)).await
    }

    async fn head(&self, key: &str) -> StorageResult<ObjectMeta> {
        validate_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || head_sync(&root, &key)).await
    }

    async fn delete(&self, key: &str) -> StorageResult<()> {
        validate_key(key)?;
        let root = self.root.clone();
        let key = key.to_string();
        blocking(move || delete_sync(&root, &key)).await
    }

    async fn list(&self, prefix: &str) -> StorageResult<Vec<ObjectMeta>> {
        validate_prefix(prefix)?;
        let root = self.root.clone();
        let prefix = prefix.to_string();
        blocking(move || list_sync(&root, &prefix)).await
    }

    async fn copy(&self, src: &str, dst: &str) -> StorageResult<()> {
        validate_key(src)?;
        validate_key(dst)?;
        let root = self.root.clone();
        let src = src.to_string();
        let dst = dst.to_string();
        blocking(move || copy_sync(&root, &src, &dst)).await
    }
}
