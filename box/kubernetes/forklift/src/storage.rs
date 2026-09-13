//! Content-addressed blob store. Blobs are immutable and keyed by their
//! SHA-256 digest, which makes concurrent reads safe across replicas sharing a
//! ReadWriteMany PersistentVolume and lets repositories share identical bytes
//! (dedup).
//!
//! Two backends implement the same traits: [`FsStore`] lays blobs out on a
//! local directory tree and [`S3BlobStore`] mirrors that layout in a bucket.
//! [`instrument`] wraps either one with latency metrics.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub mod diskusage;
pub mod instrument;
pub mod minioadmin;
pub mod s3;

pub use diskusage::{Disk, disk_usage};
pub use instrument::{InstrumentedStore, instrument};
pub use minioadmin::{MinIOInfo, minio_admin_info};
pub use s3::{S3BlobStore, S3Config, is_not_found, is_precondition_failed, new_s3_client};

/// Result alias used throughout the module.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by blob stores. Callers match [`Error::NotFound`] to map a missing
/// blob to 404 instead of 500.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The blob digest is not present in the store.
    #[error("blob not found")]
    NotFound,
    /// MinIO admin metrics cannot be collected because the backend is not a
    /// credentialed MinIO endpoint (unavailable or misconfigured).
    #[error("minio admin metrics unavailable")]
    MinIOUnavailable,
    /// A filesystem failure, with the operation it came from.
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: io::Error,
    },
    /// An S3 request failed for a reason other than a missing key.
    #[error("{op}: {source}")]
    S3 {
        op: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Wraps an I/O error with the operation that produced it.
    pub fn io(op: &'static str, source: io::Error) -> Self {
        Error::Io { op, source }
    }

    /// Wraps an SDK error with the operation that produced it.
    pub fn s3<E>(op: &'static str, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error::S3 {
            op,
            source: Box::new(source),
        }
    }
}

/// Stores and retrieves immutable, content-addressed blobs.
///
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Streams `r` into the store and returns the SHA-256 digest (hex) and the
    /// number of bytes written. Writing an already-present blob is a no-op.
    async fn put(&self, r: Pin<Box<dyn AsyncRead + Send>>) -> Result<(String, i64)>;
    /// Returns a reader for the blob identified by `digest` and its size.
    async fn open(&self, digest: &str) -> Result<(Box<dyn AsyncRead + Send + Unpin>, i64)>;
    /// Reports whether a blob is present.
    async fn exists(&self, digest: &str) -> Result<bool>;
    /// Removes a blob. Deleting a missing blob returns `Ok(())`.
    async fn delete(&self, digest: &str) -> Result<()>;
    /// The store's random-access view, when it has one. The default is `None`;
    /// wrappers forward to their inner store so wrapping never hides the
    /// capability.
    fn as_seekable(&self) -> Option<&dyn SeekableStore> {
        None
    }
}

/// A bounded staged blob suitable for archive formats whose indexes require
/// random access (ZIP). Access is blocking (`std::io`) because ZIP parsing is
/// blocking anyway; callers run it on the blocking pool. Implementations
/// release any local staging file when dropped.
pub trait ReadSeekCloser: Read + Seek + Send {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    /// The local file backing this view.
    fn path(&self) -> &Path;
}

/// Implemented by stores that can expose a blob as a local or privately staged
/// random-access file without loading it into memory.
#[async_trait]
pub trait SeekableStore: Send + Sync {
    /// Opens `digest` for random access and returns the view with its size.
    async fn open_seekable(&self, digest: &str) -> Result<(Box<dyn ReadSeekCloser>, i64)>;
}

/// A [`BlobStore`] that can also enumerate its digests in lexicographic order.
/// Replication needs the ordering to diff against a peer's cursor-paged
/// listing; ordinary blob consumers only need [`BlobStore`].
#[async_trait]
pub trait WalkableStore: BlobStore {
    /// Calls `f` for every stored blob digest in lexicographic order, stopping
    /// at the first error `f` returns.
    async fn walk_digests(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<()>;
}

/// A local file handed out by [`SeekableStore::open_seekable`].
pub struct StagedFile {
    file: std::fs::File,
    path: PathBuf,
    remove_on_drop: bool,
}

impl StagedFile {
    /// Wraps a blob file that must be left in place when the handle is dropped.
    pub fn new(file: std::fs::File, path: PathBuf) -> Self {
        StagedFile {
            file,
            path,
            remove_on_drop: false,
        }
    }

    /// Wraps a private staging copy that is removed when the handle is dropped.
    pub fn removing(file: std::fs::File, path: PathBuf) -> Self {
        StagedFile {
            file,
            path,
            remove_on_drop: true,
        }
    }
}

impl Read for StagedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for StagedFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl ReadSeekCloser for StagedFile {
    #[cfg(unix)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            // Best effort: the staging directory is private and swept on
            // restart, so a failed removal only costs disk until then.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A filesystem-backed [`BlobStore`] laid out as `<root>/blobs/aa/bb/<digest>`.
#[derive(Debug, Clone)]
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    /// Creates the blob directory under `root` and returns an `FsStore`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let base = root.as_ref().join("blobs");
        std::fs::create_dir_all(&base).map_err(|e| Error::io("create blob dir", e))?;
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&tmp).map_err(|e| Error::io("create blob tmp dir", e))?;
        Ok(FsStore { root: base })
    }

    /// Fan out by the first two byte-pairs to avoid huge directories.
    fn path(&self, digest: &str) -> PathBuf {
        self.root
            .join(&digest[0..2])
            .join(&digest[2..4])
            .join(digest)
    }
}

#[async_trait]
impl BlobStore for FsStore {
    /// Writes to a temp file while hashing, then renames into the digest path.
    /// The rename is atomic on the same filesystem so partial writes are never
    /// observed by readers.
    async fn put(&self, r: Pin<Box<dyn AsyncRead + Send>>) -> Result<(String, i64)> {
        let tmp_dir = self.root.join("tmp");
        let tmp = blocking(move || {
            tempfile::Builder::new()
                .prefix("blob-")
                .tempfile_in(tmp_dir)
        })
        .await?
        .map_err(|e| Error::io("create temp blob", e))?;
        let (std_file, tmp_path) = tmp.into_parts();
        let mut file = tokio::fs::File::from_std(std_file);

        let (digest, n) = stream_and_hash(r, &mut file, "write blob").await?;
        file.sync_all()
            .await
            .map_err(|e| Error::io("sync blob", e))?;
        drop(file);

        let dst = self.path(&digest);
        if let Some(dir) = dst.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| Error::io("create blob shard dir", e))?;
        }
        // If the blob already exists, the bytes are identical by construction;
        // keep the existing one and drop the temp file.
        if tokio::fs::metadata(&dst).await.is_ok() {
            return Ok((digest, n));
        }
        tmp_path
            .persist(&dst)
            .map_err(|e| Error::io("commit blob", e.error))?;
        Ok((digest, n))
    }

    async fn open(&self, digest: &str) -> Result<(Box<dyn AsyncRead + Send + Unpin>, i64)> {
        if !valid_digest(digest) {
            return Err(Error::NotFound);
        }
        let f = match tokio::fs::File::open(self.path(digest)).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(e) => return Err(Error::io("open blob", e)),
        };
        let meta = f.metadata().await.map_err(|e| Error::io("stat blob", e))?;
        Ok((Box::new(f), meta.len() as i64))
    }

    async fn exists(&self, digest: &str) -> Result<bool> {
        if !valid_digest(digest) {
            return Ok(false);
        }
        match tokio::fs::metadata(self.path(digest)).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io("stat blob", e)),
        }
    }

    async fn delete(&self, digest: &str) -> Result<()> {
        if !valid_digest(digest) {
            return Ok(());
        }
        match tokio::fs::remove_file(self.path(digest)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io("remove blob", e)),
        }
    }

    fn as_seekable(&self) -> Option<&dyn SeekableStore> {
        Some(self)
    }
}

#[async_trait]
impl SeekableStore for FsStore {
    /// The blob file itself is the random-access view; nothing is staged and
    /// nothing is removed when the handle is dropped.
    async fn open_seekable(&self, digest: &str) -> Result<(Box<dyn ReadSeekCloser>, i64)> {
        if !valid_digest(digest) {
            return Err(Error::NotFound);
        }
        let path = self.path(digest);
        let opened = blocking(move || {
            let file = std::fs::File::open(&path)?;
            let size = file.metadata()?.len();
            Ok::<_, io::Error>((file, path, size))
        })
        .await?;
        match opened {
            Ok((file, path, size)) => Ok((Box::new(StagedFile::new(file, path)), size as i64)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::NotFound),
            Err(e) => Err(Error::io("open blob", e)),
        }
    }
}

#[async_trait]
impl WalkableStore for FsStore {
    /// Calls `f` for every stored blob digest in lexicographic order.
    /// Replication uses the ordering to diff the local set against the
    /// leader's cursor-paged listing without holding both sets in memory.
    /// Walking stops at the first error returned by `f`.
    async fn walk_digests(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<()> {
        for d1 in sorted_dirs(&self.root).await? {
            if d1 == "tmp" {
                continue;
            }
            for d2 in sorted_dirs(&self.root.join(&d1)).await? {
                let dir = self.root.join(&d1).join(&d2);
                let mut rd = tokio::fs::read_dir(&dir)
                    .await
                    .map_err(|e| Error::io("read blob shard dir", e))?;
                let mut names = Vec::new();
                while let Some(entry) = rd
                    .next_entry()
                    .await
                    .map_err(|e| Error::io("read blob shard dir", e))?
                {
                    let ft = entry
                        .file_type()
                        .await
                        .map_err(|e| Error::io("read blob shard dir", e))?;
                    if ft.is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if valid_digest(&name) {
                        names.push(name);
                    }
                }
                names.sort();
                for name in &names {
                    f(name)?;
                }
            }
        }
        Ok(())
    }
}

/// Lists the sub-directories of `path` in lexicographic order. A missing
/// directory is an empty listing, not an error.
async fn sorted_dirs(path: &Path) -> Result<Vec<String>> {
    let mut rd = match tokio::fs::read_dir(path).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io("read blob dir", e)),
    };
    let mut out = Vec::new();
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| Error::io("read blob dir", e))?
    {
        let ft = entry
            .file_type()
            .await
            .map_err(|e| Error::io("read blob dir", e))?;
        if ft.is_dir() {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

/// Reports whether `d` is a 64-character hex SHA-256 digest. Anything else is
/// treated as "not present" by every store so a malformed digest can never
/// escape the blob layout.
pub(crate) fn valid_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Copies `r` into `file` while hashing, returning the hex digest and byte
/// count. `op` names the failing step in errors ("write blob", "buffer blob").
pub(crate) async fn stream_and_hash(
    mut r: Pin<Box<dyn AsyncRead + Send>>,
    file: &mut tokio::fs::File,
    op: &'static str,
) -> Result<(String, i64)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n: i64 = 0;
    loop {
        let read = r.read(&mut buf).await.map_err(|e| Error::io(op, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read])
            .await
            .map_err(|e| Error::io(op, e))?;
        n += read as i64;
    }
    file.flush().await.map_err(|e| Error::io(op, e))?;
    Ok((hex::encode(hasher.finalize()), n))
}

/// Runs a short blocking filesystem call on the blocking pool.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Other(format!("blob store task failed: {e}")))
}

#[cfg(test)]
pub(crate) mod tests {
    mod fsstore_more {
        use std::io::SeekFrom;

        use crate::storage::*;

        fn reader(data: &[u8]) -> Pin<Box<dyn AsyncRead + Send>> {
            Box::pin(std::io::Cursor::new(data.to_vec()))
        }

        #[tokio::test]
        async fn fs_store_round_trip_and_delete() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            let data = b"hello forklift fs store";
            let want_hex = hex::encode(Sha256::digest(data));

            let (digest, n) = s.put(reader(data)).await.unwrap();
            assert_eq!((digest.as_str(), n), (want_hex.as_str(), data.len() as i64));
            // Re-putting the same content dedups to the same digest.
            let (d2, _) = s.put(reader(data)).await.unwrap();
            assert_eq!(d2, want_hex, "dedup put");

            assert!(s.exists(&digest).await.unwrap(), "exists");

            let (mut rc, size) = s.open(&digest).await.unwrap();
            assert_eq!(size, data.len() as i64, "open size");
            let mut got = Vec::new();
            rc.read_to_end(&mut got).await.unwrap();
            drop(rc);
            assert_eq!(got, data, "read");

            // Seekable open supports random access.
            let (mut sc, ssize) = s.open_seekable(&digest).await.unwrap();
            assert_eq!(ssize, data.len() as i64, "openseekable size");
            sc.seek(SeekFrom::Start(6)).expect("seek");
            let mut seeked = Vec::new();
            sc.read_to_end(&mut seeked).unwrap();
            let mut at = vec![0u8; 5];
            sc.read_at(&mut at, 0).unwrap();
            drop(sc);
            assert_eq!(seeked, &data[6..], "seeked read");
            assert_eq!(at, &data[..5], "read_at");

            let mut walked: Vec<String> = Vec::new();
            s.walk_digests(&mut |d| {
                walked.push(d.to_string());
                Ok(())
            })
            .await
            .expect("walk");
            assert_eq!(walked, vec![want_hex], "walk");

            s.delete(&digest).await.expect("delete");
            assert!(
                !s.exists(&digest).await.unwrap(),
                "blob still exists after delete"
            );
        }
    }

    mod fsstore {
        use crate::storage::*;

        /// Wraps bytes as the `Pin<Box<dyn AsyncRead + Send>>` `put` takes.
        fn reader(data: &[u8]) -> Pin<Box<dyn AsyncRead + Send>> {
            Box::pin(std::io::Cursor::new(data.to_vec()))
        }

        #[tokio::test]
        async fn fs_store_round_trip() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).expect("new store");

            let data = b"hello forklift";
            let want_hex = hex::encode(Sha256::digest(data));

            let (digest, n) = s.put(reader(data)).await.expect("put");
            assert_eq!(digest, want_hex, "digest");
            assert_eq!(n, data.len() as i64, "size");

            assert!(s.exists(&digest).await.unwrap(), "exists");

            let (mut rc, size) = s.open(&digest).await.expect("open");
            assert_eq!(size, data.len() as i64, "open size");
            let mut got = Vec::new();
            rc.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, data, "read");
        }

        #[tokio::test]
        async fn fs_store_dedup() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            let (d1, _) = s.put(reader(b"same bytes")).await.unwrap();
            let (d2, _) = s.put(reader(b"same bytes")).await.unwrap();
            assert_eq!(d1, d2, "expected identical digests");
        }

        #[tokio::test]
        async fn fs_store_delete() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            let (digest, _) = s.put(reader(b"to delete")).await.unwrap();
            s.delete(&digest).await.expect("delete");
            assert!(!s.exists(&digest).await.unwrap(), "blob should be gone");
            // Deleting a missing blob is a no-op.
            s.delete(&digest).await.expect("delete missing");
        }

        #[tokio::test]
        async fn fs_store_open_missing() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            let err = s.open(&"a".repeat(64)).await.map(|(_, n)| n).unwrap_err();
            assert!(matches!(err, Error::NotFound), "err = {err:?}");
            // Invalid digests are treated as not found, not errors.
            let err = s.open("short").await.map(|(_, n)| n).unwrap_err();
            assert!(
                matches!(err, Error::NotFound),
                "invalid digest err = {err:?}"
            );
            assert!(
                !s.exists("bad").await.unwrap(),
                "invalid digest should not exist"
            );
        }
    }

    mod walk {
        use crate::storage::*;

        fn reader(data: Vec<u8>) -> Pin<Box<dyn AsyncRead + Send>> {
            Box::pin(std::io::Cursor::new(data))
        }

        #[tokio::test]
        async fn walk_digests_ordered_and_skips_tmp() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();

            let mut want = Vec::with_capacity(5);
            for i in 0..5 {
                let (d, _) = s
                    .put(reader(format!("walk-{i}").into_bytes()))
                    .await
                    .unwrap();
                want.push(d);
            }
            want.sort();

            let mut got: Vec<String> = Vec::new();
            s.walk_digests(&mut |d| {
                got.push(d.to_string());
                Ok(())
            })
            .await
            .expect("walk");
            assert_eq!(got.len(), want.len(), "digest count");
            assert_eq!(got, want, "order mismatch");
        }

        #[tokio::test]
        async fn walk_digests_stops_on_callback_error() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            for i in 0..3 {
                s.put(reader(format!("stop-{i}").into_bytes()))
                    .await
                    .unwrap();
            }
            let mut calls = 0;
            let err = s
                .walk_digests(&mut |_| {
                    calls += 1;
                    Err(Error::Other("stop".into()))
                })
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), "stop", "err = {err:?}");
            assert_eq!(calls, 1, "calls");
        }

        #[tokio::test]
        async fn walk_digests_empty_store() {
            let dir = tempfile::tempdir().unwrap();
            let s = FsStore::new(dir.path()).unwrap();
            s.walk_digests(&mut |_| panic!("unexpected digest in empty store"))
                .await
                .expect("walk");
        }
    }
}
