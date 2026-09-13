//! SQLite-backed metadata store: repositories, artifacts, blob reference
//! counts, users, roles, tokens, approvals, scans and everything else the
//! console and the package protocols persist.
//!
//! The store wraps two connection sets over the same database file: a
//! single-connection *write* pool that preserves the one-writer discipline and
//! a small *read* pool so WAL's concurrent readers are actually usable.
//! Without the split every hot-path lookup would serialize behind whichever
//! statement holds the sole connection (including multi-second maintenance
//! work like `VACUUM INTO`). Both sets sit behind an [`arc_swap::ArcSwap`] so
//! PV-based replication can atomically swap in a replicated snapshot when a
//! standby is promoted to leader (see [`Store::swap_from_snapshot`]).
//!
//! SQLite is synchronous, so every statement runs on the blocking thread pool via
//! [`tokio::task::spawn_blocking`]; the `async fn` surface exposed to the rest of the crate
//! hides that. WAL readers see the latest commit, so a read immediately after a write observes
//! it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::Semaphore;

mod announcement;
mod approval;
mod artifact;
mod audit;
mod auth;
mod blob;
mod coverage;
mod group_metadata_cache;
mod label;
mod license;
mod migrations;
pub mod models;
mod oci;
mod publication;
mod rbac;
mod receiver;
mod repository;
mod search;
pub mod time;
mod versiondeny;
mod vuln;

pub use announcement::*;
pub use approval::*;
pub use artifact::*;
pub use audit::*;
pub use label::*;
pub use license::*;
pub use models::*;
pub use oci::*;
pub use publication::*;
pub use rbac::*;
pub use receiver::*;
pub use search::*;
pub use time::{format_time, format_time_opt, now_rfc3339, parse_time, parse_time_opt};
pub use versiondeny::*;
pub use vuln::*;

/// Store errors with distinct variants for HTTP status mapping.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A row does not exist.
    #[error("not found")]
    NotFound,
    /// A uniqueness constraint was violated (name already taken, duplicate row).
    #[error("conflict")]
    Conflict,
    #[error("managed artifact")]
    ManagedArtifact,
    /// A durable upload request changed state under the caller.
    #[error("upload state changed")]
    UploadState,
    /// A publication asset collides with an existing artifact at the same path.
    #[error("artifact conflict")]
    ArtifactConflict,
    /// A shared metadata index was rewritten concurrently.
    #[error("derived metadata changed")]
    DerivedMetadataChanged,
    /// An artifact already carries the maximum number of labels.
    #[error("artifact label limit reached")]
    LabelLimit,
    /// Any SQLite failure not classified above, with the operation it came from.
    #[error("{op}: {source}")]
    Sqlite {
        op: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    /// A filesystem failure (snapshot files, swaps).
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// Malformed JSON in a stored column.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// The blocking task carrying a statement was cancelled or panicked.
    #[error("database task failed: {0}")]
    Task(String),
    /// Free-form failure with a message.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Wraps a rusqlite error, translating the row-missing and constraint
    /// classes into their sentinel variants so callers can `match` on them.
    pub fn sqlite(op: &'static str, source: rusqlite::Error) -> Self {
        match &source {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound,
            rusqlite::Error::SqliteFailure(e, _)
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Error::Conflict
            }
            _ => Error::Sqlite { op, source },
        }
    }

    /// Wraps an I/O error with the operation that failed.
    pub fn io(op: &'static str, source: std::io::Error) -> Self {
        Error::Io { op, source }
    }

    /// True for [`Error::NotFound`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::NotFound)
    }

    /// True for [`Error::Conflict`].
    pub fn is_conflict(&self) -> bool {
        matches!(self, Error::Conflict)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::sqlite("sqlite", e)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Task(e.to_string())
    }
}

/// Result alias used throughout the store.
pub type Result<T> = std::result::Result<T, Error>;

/// Bounds a single migration pass so a stuck migration cannot hang startup
/// forever. Applying the embedded migrations is otherwise fast; the ceiling
/// only trips on pathological disk stalls.
const MIGRATE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounds the read pool. Metadata point lookups are microseconds long, so a
/// handful of connections removes head-of-line blocking without meaningfully
/// raising memory or file-handle cost.
pub const READ_POOL_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Maximum connections the pool hands out.
    pub max_open: u64,
    /// Connections currently held by a statement.
    pub in_use: u64,
    /// Connections currently free.
    pub idle: u64,
    /// Number of times a caller waited for a connection.
    pub wait_count: u64,
    /// Total time callers spent waiting for a connection.
    pub wait_duration: Duration,
}

/// A pool of `Connection`s over one database file. `size == 1` is the write
/// pool; the read pool holds [`READ_POOL_SIZE`].
struct Pool {
    conns: parking_lot::Mutex<Vec<Connection>>,
    permits: Semaphore,
    size: usize,
    in_use: AtomicU64,
    wait_count: AtomicU64,
    wait_nanos: AtomicU64,
    /// Set once the pool has been closed by a swap; connections returned late
    /// are dropped instead of being re-pooled.
    closed: std::sync::atomic::AtomicBool,
}

impl Pool {
    fn open(path: &Path, size: usize) -> Result<Arc<Pool>> {
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(open_connection(path)?);
        }
        Ok(Arc::new(Pool {
            conns: parking_lot::Mutex::new(conns),
            permits: Semaphore::new(size),
            size,
            in_use: AtomicU64::new(0),
            wait_count: AtomicU64::new(0),
            wait_nanos: AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Runs `f` on a pooled connection on the blocking thread pool.
    async fn run<T, F>(self: &Arc<Self>, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let mut lease = PoolConnection {
            pool: Arc::clone(self),
            conn: Some(self.acquire().await?),
        };
        // The blocking task owns the lease, so cancellation of its async
        // caller cannot discard the connection or leak a semaphore permit.
        tokio::task::spawn_blocking(move || {
            let conn = lease
                .conn
                .as_mut()
                .ok_or_else(|| Error::Other("database lease closed".into()))?;
            f(conn)
        })
        .await?
    }

    /// Takes a connection out of the pool, recording the wait when none is free.
    async fn acquire(self: &Arc<Self>) -> Result<Connection> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Other("database handle closed".into()));
        }
        let permit = match self.permits.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                let started = Instant::now();
                let p = self
                    .permits
                    .acquire()
                    .await
                    .map_err(|_| Error::Other("database handle closed".into()))?;
                self.wait_count.fetch_add(1, Ordering::Relaxed);
                self.wait_nanos
                    .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                p
            }
        };
        // The permit guards the count; the connection itself travels with the
        // blocking task, so forget the permit and re-add it on release.
        permit.forget();
        let conn = self
            .conns
            .lock()
            .pop()
            .ok_or_else(|| Error::Other("database pool exhausted".into()))?;
        self.in_use.fetch_add(1, Ordering::Relaxed);
        Ok(conn)
    }

    fn release(&self, conn: Connection) {
        self.in_use.fetch_sub(1, Ordering::Relaxed);
        if self.closed.load(Ordering::Acquire) {
            drop(conn);
            return;
        }
        self.conns.lock().push(conn);
        self.permits.add_permits(1);
    }

    /// Closes the pool: no further acquisitions succeed and pooled connections
    /// are dropped. Connections held by in-flight statements are dropped when
    /// they return.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.permits.close();
        self.conns.lock().clear();
    }

    fn stats(&self) -> PoolStats {
        let in_use = self.in_use.load(Ordering::Relaxed);
        PoolStats {
            max_open: self.size as u64,
            in_use,
            idle: (self.size as u64).saturating_sub(in_use),
            wait_count: self.wait_count.load(Ordering::Relaxed),
            wait_duration: Duration::from_nanos(self.wait_nanos.load(Ordering::Relaxed)),
        }
    }
}

/// Returns a checked-out connection even if a blocking task unwinds or its
/// caller stops waiting for it. The connection stays checked out until the
/// blocking operation has actually finished.
struct PoolConnection {
    pool: Arc<Pool>,
    conn: Option<Connection>,
}

impl Drop for PoolConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.release(conn);
        }
    }
}

/// The current write and read pools; replaced wholesale by a snapshot swap.
struct Handles {
    write: Arc<Pool>,
    read: Arc<Pool>,
}

/// The metadata store. Cheap to share behind an [`Arc`]; every method takes
/// `&self`.
pub struct Store {
    handles: ArcSwap<Handles>,
    path: PathBuf,
}

/// Opens one connection with the pragmas every handle needs.
///
/// `temp_store = 2` keeps statement journals and temp b-trees in memory: the
/// scratch container has no `/tmp` (and `readOnlyRootFilesystem` forbids
/// creating one), so any file-backed temp object would fail with
/// `SQLITE_IOERR_GETTEMPPATH`.
fn open_connection(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn =
        Connection::open_with_flags(path, flags).map_err(|e| Error::sqlite("open sqlite", e))?;
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(|e| Error::sqlite("set busy timeout", e))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = 2;",
    )
    .map_err(|e| Error::sqlite("apply pragmas", e))?;
    Ok(conn)
}

impl Store {
    /// Opens (creating if needed) the SQLite database at `path` and applies any
    /// pending migrations. WAL mode and a busy timeout reduce lock contention;
    /// the single-writer guarantee for HA is provided by leader election, not
    /// the database.
    pub async fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        let handles = open_handles(&path).await?;
        Ok(Store {
            handles: ArcSwap::from_pointee(handles),
            path,
        })
    }

    /// Opens a store on a fresh temporary file. Test helper.
    pub async fn open_temp() -> Result<(Store, tempfile::TempDir)> {
        let dir = tempfile::tempdir().map_err(|e| Error::io("create temp dir", e))?;
        let store = Store::open(dir.path().join("forklift.db")).await?;
        Ok((store, dir))
    }

    /// The database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs a mutating statement (or transaction) on the single write
    /// connection. `f` receives the connection and may open a transaction with
    /// `conn.transaction()`; return the store [`Error`] it maps to.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let handles = self.handles.load();
        let pool = Arc::clone(&handles.write);
        drop(handles);
        pool.run(f).await
    }

    /// Runs a read-only statement on the read pool.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let handles = self.handles.load();
        let pool = Arc::clone(&handles.read);
        drop(handles);
        pool.run(f).await
    }

    /// Closes both pools. Statements in flight finish on their connections,
    /// which are then dropped instead of re-pooled.
    pub fn close(&self) {
        let handles = self.handles.load();
        handles.read.close();
        handles.write.close();
    }

    /// Connection-pool statistics for the write and read pools, in that order.
    ///
    /// Every write serializes on one connection, so the signal that matters is
    /// not CPU or query count but how long callers wait for that connection:
    /// `wait_count` and `wait_duration` rising is write-pool saturation, which
    /// is what makes everything look slow at once.
    pub fn pool_stats(&self) -> (PoolStats, PoolStats) {
        let handles = self.handles.load();
        (handles.write.stats(), handles.read.stats())
    }

    /// Verifies database connectivity through the read pool.
    ///
    /// The pool matters: the write handle is a single connection, so pinging it
    /// would put the readiness probe behind whatever statement currently holds
    /// it, and a probe with a one-second timeout then fails during ordinary
    /// write contention.
    pub async fn ping(&self) -> Result<()> {
        self.read(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))
                .map_err(|e| Error::sqlite("ping", e))
        })
        .await
    }

    /// Writes a consistent point-in-time copy of the database to `dst` using
    /// `VACUUM INTO`. The destination must not exist; any stale file is removed
    /// first. The read pool runs the vacuum: it is a long read transaction, and
    /// keeping it off the write connection stops a multi-second snapshot from
    /// blocking every write while it copies.
    pub async fn snapshot(&self, dst: impl AsRef<Path>) -> Result<()> {
        let dst = dst.as_ref().to_path_buf();
        match tokio::fs::remove_file(&dst).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io("remove stale snapshot", e)),
        }
        let dst_str = dst.to_string_lossy().into_owned();
        self.read(move |conn| {
            conn.execute("VACUUM INTO ?1", [dst_str])
                .map(|_| ())
                .map_err(|e| Error::sqlite("vacuum into", e))
        })
        .await
    }

    /// Pins a read-pool connection for change detection. See [`ChangeWatch`].
    pub async fn new_change_watch(&self) -> Result<ChangeWatch> {
        let handles = self.handles.load();
        let pool = Arc::clone(&handles.read);
        drop(handles);
        let conn = pool.acquire().await?;
        Ok(ChangeWatch {
            pool: Arc::clone(&pool),
            conn: Arc::new(parking_lot::Mutex::new(Some(PoolConnection {
                pool,
                conn: Some(conn),
            }))),
        })
    }

    /// Atomically replaces the database file with the snapshot at
    /// `snapshot_path` and reopens the handles. Used when a replication standby
    /// is promoted to leader: the standby's local database is discarded in
    /// favor of the snapshot pulled from the previous leader. The store must
    /// not be serving write traffic when this is called (the standby is not
    /// Ready).
    pub async fn swap_from_snapshot(&self, snapshot_path: impl AsRef<Path>) -> Result<()> {
        let snapshot_path = snapshot_path.as_ref().to_path_buf();
        let old = self.handles.load_full();
        old.read.close();
        old.write.close();
        // Drop WAL sidecar files belonging to the old database before the
        // rename so SQLite never pairs the new file with a stale WAL.
        for suffix in ["-wal", "-shm"] {
            let mut side = self.path.as_os_str().to_owned();
            side.push(suffix);
            match tokio::fs::remove_file(PathBuf::from(side)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io("remove wal sidecar", e)),
            }
        }
        tokio::fs::rename(&snapshot_path, &self.path)
            .await
            .map_err(|e| Error::io("replace db file", e))?;
        let handles = open_handles(&self.path).await?;
        self.handles.store(Arc::new(handles));
        Ok(())
    }
}

/// Opens the write pool, migrates, then opens the read pool so it never
/// observes a half-migrated schema.
async fn open_handles(path: &Path) -> Result<Handles> {
    let write = Pool::open(path, 1)?;
    // Migrations are a startup-critical, atomic step: a shutdown signal must not
    // abort one mid-apply and leave the schema half-migrated. The blocking task
    // is not cancellable, which is what we want; the timeout still bounds it.
    let migrate = write.run(migrate);
    match tokio::time::timeout(MIGRATE_TIMEOUT, migrate).await {
        Ok(res) => res?,
        Err(_) => return Err(Error::Other("migrate: timed out".into())),
    }
    let read = Pool::open(path, READ_POOL_SIZE)?;
    Ok(Handles { write, read })
}

/// Applies every embedded migration not yet recorded in `schema_migrations`,
/// each in its own transaction.
fn migrate(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        )",
    )
    .map_err(|e| Error::sqlite("create schema_migrations", e))?;

    let applied: std::collections::HashSet<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migrations")
            .map_err(|e| Error::sqlite("list migrations", e))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| Error::sqlite("list migrations", e))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(|e| Error::sqlite("list migrations", e))?
    };

    for (name, body) in migrations::MIGRATIONS {
        let version = migration_version(name)?;
        if applied.contains(&version) {
            continue;
        }
        let tx = conn
            .transaction()
            .map_err(|e| Error::sqlite("begin migration", e))?;
        tx.execute_batch(body)
            .map_err(|e| Error::Other(format!("apply {name}: {e}")))?;
        tx.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES(?1, ?2)",
            rusqlite::params![version, now_rfc3339()],
        )
        .map_err(|e| Error::sqlite("record migration", e))?;
        tx.commit()
            .map_err(|e| Error::sqlite("commit migration", e))?;
    }
    Ok(())
}

/// Parses the numeric prefix of a migration file name like `0001_init.sql`.
fn migration_version(name: &str) -> Result<i64> {
    let idx = name
        .find('_')
        .filter(|&i| i > 0)
        .ok_or_else(|| Error::Other(format!("bad migration name {name:?}")))?;
    name[..idx]
        .parse::<i64>()
        .map_err(|e| Error::Other(format!("bad migration version in {name:?}: {e}")))
}

/// Reports whether any connection has committed to the database since the
/// previous check. It pins one read-pool connection for its lifetime because
/// SQLite's `data_version` counter is per connection: two reads on the same
/// connection differ exactly when another connection committed in between,
/// and values read on different connections are not comparable at all. The
/// pinned connection is taken out of the read pool, so hold a watch only while
/// it is needed (the s3 leader holds one; standbys hold none).
pub struct ChangeWatch {
    pool: Arc<Pool>,
    conn: Arc<parking_lot::Mutex<Option<PoolConnection>>>,
}

impl ChangeWatch {
    /// Returns the pinned connection's `data_version`. The value itself is
    /// opaque; only inequality between two calls carries meaning. After
    /// [`Store::swap_from_snapshot`] the underlying pool is closed and this
    /// fails; the caller then closes the watch and opens a new one.
    pub async fn version(&self) -> Result<i64> {
        if self.pool.closed.load(Ordering::Acquire) {
            return Err(Error::Other("read data_version: handle closed".into()));
        }
        let slot = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut slot = slot.lock();
            let conn = slot
                .as_mut()
                .and_then(|lease| lease.conn.as_mut())
                .ok_or_else(|| Error::Other("change watch closed".into()))?;
            conn.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
                .map_err(|e| Error::sqlite("read data_version", e))
        })
        .await?
    }

    /// Returns the pinned connection to the pool.
    pub fn close(&self) {
        self.conn.lock().take();
    }
}

impl Drop for ChangeWatch {
    fn drop(&mut self) {
        self.close();
    }
}

/// Re-exported so the table-level unit tests in this module tree keep
/// reaching for `test_store()` without importing the harness by path.
#[cfg(test)]
pub(crate) use crate::testing::meta::test_store;

#[cfg(test)]
pub(crate) mod tests {
    mod readpool {
        //!
        //! The write pool is a single connection by design (single-writer SQLite), so
        //! any read that runs on it waits for whatever statement holds it — and that
        //! included the readiness probe, which turned ordinary write contention into a
        //! pod dropped from the Service.
        //!
        //! The test holds the write connection in an open transaction and then
        //! exercises the read paths with a deadline far shorter than the transaction.
        //! On the write pool every one of these would block until the transaction
        //! ends; on the read pool they answer immediately.

        use std::sync::Arc;
        use std::time::Duration;

        use crate::meta::*;

        #[tokio::test]
        async fn cancelled_query_returns_its_connection_after_it_finishes() {
            let (_store, dir) = crate::testing::meta::test_store().await;
            let pool = Pool::open(&dir.path().join("forklift.db"), 1).unwrap();
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let held_pool = Arc::clone(&pool);
            let query = tokio::spawn(async move {
                held_pool
                    .run(move |_| {
                        started.send(()).unwrap();
                        released.blocking_recv().unwrap();
                        Ok(())
                    })
                    .await
            });
            running.await.unwrap();
            query.abort();
            assert!(query.await.unwrap_err().is_cancelled());
            assert_eq!(pool.stats().in_use, 1, "the query is still running");
            release.send(()).unwrap();
            with_deadline("connection after cancellation", pool.run(|_| Ok(()))).await;
            assert_eq!(pool.stats().in_use, 0);
        }

        #[tokio::test]
        async fn panicking_query_returns_its_connection() {
            let (_store, dir) = crate::testing::meta::test_store().await;
            let pool = Pool::open(&dir.path().join("forklift.db"), 1).unwrap();
            let result: Result<()> = pool.run(|_| panic!("test query panic")).await;
            assert!(matches!(result, Err(Error::Task(_))));
            with_deadline("connection after panic", pool.run(|_| Ok(()))).await;
            assert_eq!(pool.stats().in_use, 0);
        }

        #[test]
        fn cancelled_change_watch_query_keeps_its_pinned_connection() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (store, _dir) = crate::testing::meta::test_store().await;
                let watch = store.new_change_watch().await.unwrap();
                let (started, running) = tokio::sync::oneshot::channel();
                let (release, released) = tokio::sync::oneshot::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    released.blocking_recv().unwrap();
                });
                running.await.unwrap();
                // The query is queued on the occupied blocking pool when it times out.
                let cancelled =
                    tokio::time::timeout(Duration::from_millis(20), watch.version()).await;
                release.send(()).unwrap();
                blocker.await.unwrap();
                assert!(cancelled.is_err());
                with_deadline("change watch after cancellation", async {
                    watch.version().await.map(|_| ())
                })
                .await;
                watch.close();
                assert_eq!(store.pool_stats().1.in_use, 0);
            });
        }

        #[tokio::test]
        async fn reads_do_not_wait_for_the_write_connection() {
            let (store, _dir) = crate::testing::meta::test_store().await;
            let store = Arc::new(store);

            let repo = store
                .create_repository(Repository {
                    name: "npm-hosted".into(),
                    format: FORMAT_NPM.into(),
                    r#type: TYPE_HOSTED.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();
            store
                .put_artifact(Artifact {
                    repo_id: repo.id,
                    path: "lodash/-/lodash-4.17.21.tgz".into(),
                    version: "4.17.21".into(),
                    blob_sha256: "sha-a".into(),
                    size: 10,
                    ..Artifact::default()
                })
                .await
                .unwrap();

            // Occupy the write connection for longer than any deadline below.
            let (release, released) = tokio::sync::oneshot::channel::<()>();
            let holder = Arc::clone(&store);
            let held = tokio::spawn(async move {
                holder
                    .write(move |c| {
                        c.execute(
                            "INSERT INTO blobs(sha256, size, ref_count, created_at) VALUES('sha-held', 1, 0, ?)",
                            rusqlite::params![now_rfc3339()],
                        )
                        .map_err(|e| Error::sqlite("write inside the held transaction", e))?;
                        let _ = released.blocking_recv();
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            // Let the holding task actually take the connection.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // The readiness probe: the failure that took the instance out of service.
            with_deadline("ping", store.ping()).await;
            with_deadline("list_repositories", async {
                store.list_repositories().await.map(|_| ())
            })
            .await;
            // The repository list's two whole-table aggregates.
            with_deadline("all_repo_stats", async {
                store.all_repo_stats().await.map(|_| ())
            })
            .await;
            with_deadline("all_scan_targets", async {
                store.all_scan_targets().await.map(|_| ())
            })
            .await;
            // The console's own reads.
            with_deadline("search_repo_artifacts", async {
                store
                    .search_repo_artifacts(repo.id, "lodash", 10, 0)
                    .await
                    .map(|_| ())
            })
            .await;
            with_deadline("blob_stats", async { store.blob_stats().await.map(|_| ()) }).await;

            let _ = release.send(());
            held.await.unwrap();
        }

        /// Fails the test when `call` does not answer well inside the window the held
        /// write transaction stays open for.
        async fn with_deadline(name: &str, call: impl Future<Output = Result<()>>) {
            match tokio::time::timeout(Duration::from_secs(2), call).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => panic!("{name} failed: {e}"),
                Err(_) => panic!("{name} blocked behind the open write transaction"),
            }
        }
    }

    mod store {
        //! Table-level tests live next to the modules that own the tables.

        use chrono::{TimeZone, Utc};

        use crate::meta::*;

        #[tokio::test]
        async fn open_applies_every_migration_once() {
            let (store, dir) = crate::testing::meta::test_store().await;
            let applied: i64 = store
                .read(|c| {
                    c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                        .map_err(|e| Error::sqlite("count", e))
                })
                .await
                .unwrap();
            assert_eq!(applied as usize, migrations::MIGRATIONS.len());
            store.close();

            // Reopening is idempotent: nothing is re-applied.
            let store = Store::open(dir.path().join("forklift.db")).await.unwrap();
            let again: i64 = store
                .read(|c| {
                    c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                        .map_err(|e| Error::sqlite("count", e))
                })
                .await
                .unwrap();
            assert_eq!(again, applied);
            store.ping().await.unwrap();
        }

        #[tokio::test]
        async fn migration_version_parses_prefix() {
            assert_eq!(migration_version("0001_init.sql").unwrap(), 1);
            assert_eq!(
                migration_version("0038_coverage_mute_scopes.sql").unwrap(),
                38
            );
            assert!(migration_version("init.sql").is_err());
            assert!(migration_version("_x.sql").is_err());
            assert!(migration_version("ab_x.sql").is_err());
        }

        #[tokio::test]
        async fn write_errors_map_to_sentinels() {
            let (store, _dir) = crate::testing::meta::test_store().await;
            let e = store
                .read(|c| {
                    c.query_row("SELECT id FROM repositories WHERE id = -1", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map_err(|e| Error::sqlite("get", e))
                })
                .await
                .unwrap_err();
            assert!(e.is_not_found(), "{e}");

            let ins = |c: &mut Connection| {
                c.execute(
                    "INSERT INTO repositories(name, format, type, upstream_url, config_json, created_at, updated_at)
                     VALUES('dup', 'maven', 'hosted', '', '{}', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                    [],
                )
                .map(|_| ())
                .map_err(|e| Error::sqlite("insert", e))
            };
            store.write(ins).await.unwrap();
            let e = store.write(ins).await.unwrap_err();
            assert!(e.is_conflict(), "{e}");
        }

        #[tokio::test]
        async fn snapshot_and_swap_replace_database() {
            let (store, dir) = crate::testing::meta::test_store().await;
            store
                .write(|c| {
                    c.execute(
                        "INSERT INTO repositories(name, format, type, upstream_url, config_json, created_at, updated_at)
                         VALUES('keep', 'npm', 'hosted', '', '{}', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                        [],
                    )
                    .map(|_| ())
                    .map_err(|e| Error::sqlite("insert", e))
                })
                .await
                .unwrap();
            let snap = dir.path().join("snap.db");
            store.snapshot(&snap).await.unwrap();
            assert!(snap.exists());
            // A stale file at the destination is removed first.
            store.snapshot(&snap).await.unwrap();

            // Mutate the live database after the snapshot, then swap the snapshot in:
            // the later row must be gone.
            store
                .write(|c| {
                    c.execute(
                        "INSERT INTO repositories(name, format, type, upstream_url, config_json, created_at, updated_at)
                         VALUES('lost', 'npm', 'hosted', '', '{}', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                        [],
                    )
                    .map(|_| ())
                    .map_err(|e| Error::sqlite("insert", e))
                })
                .await
                .unwrap();
            store.swap_from_snapshot(&snap).await.unwrap();
            let names: Vec<String> = store
                .read(|c| {
                    let mut s = c.prepare("SELECT name FROM repositories ORDER BY name")?;
                    let v = s
                        .query_map([], |r| r.get(0))?
                        .collect::<std::result::Result<_, _>>()?;
                    Ok(v)
                })
                .await
                .unwrap();
            assert_eq!(names, vec!["keep".to_string()]);
            assert!(!snap.exists(), "snapshot file is renamed into place");
            store.ping().await.unwrap();
        }

        #[tokio::test]
        async fn change_watch_detects_commits_on_other_connections() {
            let (store, _dir) = crate::testing::meta::test_store().await;
            let watch = store.new_change_watch().await.unwrap();
            let v1 = watch.version().await.unwrap();
            let v2 = watch.version().await.unwrap();
            assert_eq!(v1, v2, "no commit in between");
            store
                .write(|c| {
                    c.execute(
                        "INSERT INTO repositories(name, format, type, upstream_url, config_json, created_at, updated_at)
                         VALUES('w', 'npm', 'hosted', '', '{}', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
                        [],
                    )
                    .map(|_| ())
                    .map_err(|e| Error::sqlite("insert", e))
                })
                .await
                .unwrap();
            let v3 = watch.version().await.unwrap();
            assert_ne!(v2, v3, "a commit elsewhere changes data_version");

            // The pinned connection leaves the pool while held and returns on close.
            let (_, read) = store.pool_stats();
            assert_eq!(read.in_use, 1);
            watch.close();
            let (_, read) = store.pool_stats();
            assert_eq!(read.in_use, 0);
            assert_eq!(read.max_open, READ_POOL_SIZE as u64);
        }

        #[tokio::test]
        async fn change_watch_fails_after_swap() {
            let (store, dir) = crate::testing::meta::test_store().await;
            let watch = store.new_change_watch().await.unwrap();
            let snap = dir.path().join("snap.db");
            store.snapshot(&snap).await.unwrap();
            store.swap_from_snapshot(&snap).await.unwrap();
            assert!(watch.version().await.is_err());
            // A new watch on the reopened pool works.
            let watch = store.new_change_watch().await.unwrap();
            watch.version().await.unwrap();
        }

        #[tokio::test]
        async fn pool_stats_count_waits_on_the_single_writer() {
            let (store, _dir) = crate::testing::meta::test_store().await;
            let store = std::sync::Arc::new(store);
            let (w, _) = store.pool_stats();
            assert_eq!(w.max_open, 1);
            assert_eq!(w.wait_count, 0);
            // Hold the writer in one task while another queues behind it.
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let s1 = std::sync::Arc::clone(&store);
            let hold = tokio::spawn(async move {
                s1.write(move |_c| {
                    let _ = rx.blocking_recv();
                    Ok(())
                })
                .await
                .unwrap();
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
            let s2 = std::sync::Arc::clone(&store);
            let waiter = tokio::spawn(async move { s2.write(|_c| Ok(())).await.unwrap() });
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(());
            hold.await.unwrap();
            waiter.await.unwrap();
            let (w, _) = store.pool_stats();
            assert_eq!(w.wait_count, 1);
            assert!(w.wait_duration > Duration::ZERO);
            assert_eq!(w.in_use, 0);
        }

        #[tokio::test]
        async fn close_refuses_further_statements() {
            let (store, _dir) = crate::testing::meta::test_store().await;
            store.close();
            assert!(store.ping().await.is_err());
        }

        #[tokio::test]
        async fn migrate_idempotent() {
            let dir = tempfile::tempdir().unwrap();
            let s1 = Store::open(dir.path().join("db.sqlite")).await.unwrap();
            s1.close();
            // Reopening applies no migrations and must not error.
            let s2 = Store::open(dir.path().join("db.sqlite")).await.unwrap();
            s2.close();
        }

        #[tokio::test]
        async fn repository_crud() {
            let (s, _dir) = crate::testing::meta::test_store().await;

            let r = s
                .create_repository(Repository {
                    name: "maven-central".into(),
                    format: FORMAT_MAVEN.into(),
                    r#type: TYPE_PROXY.into(),
                    upstream_url: "https://repo1.maven.org/maven2".into(),
                    ..Repository::default()
                })
                .await
                .expect("create");
            assert!(r.id != 0 && r.config_json == "{}", "unexpected repo: {r:?}");

            let got = s
                .get_repository_by_name("maven-central")
                .await
                .expect("get by name");
            assert_eq!(got.id, r.id);

            s.update_repository_config(
                r.id,
                "https://example.com",
                r#"{"cache":{"enabled":false}}"#,
            )
            .await
            .expect("update");
            let got = s.get_repository(r.id).await.unwrap();
            assert_eq!(
                got.upstream_url, "https://example.com",
                "upstream not updated"
            );

            let list = s.list_repositories().await.expect("list");
            assert_eq!(list.len(), 1);

            s.delete_repository(r.id).await.expect("delete");
            let err = s.get_repository(r.id).await.unwrap_err();
            assert!(err.is_not_found(), "want NotFound, got {err}");
        }

        #[tokio::test]
        async fn artifact_ref_counting() {
            let (s, _dir) = crate::testing::meta::test_store().await;
            let repo = s
                .create_repository(Repository {
                    name: "r".into(),
                    format: FORMAT_NPM.into(),
                    r#type: TYPE_HOSTED.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();

            let pub_at = chrono::Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
            let a = s
                .put_artifact(Artifact {
                    repo_id: repo.id,
                    path: "a/1.0/a-1.0.tgz".into(),
                    version: "1.0".into(),
                    blob_sha256: "blobA".into(),
                    size: 10,
                    published_at: Some(pub_at),
                    ..Artifact::default()
                })
                .await
                .expect("put");
            assert_eq!(a.published_at, Some(pub_at), "published_at not persisted");
            assert_eq!(s.get_blob("blobA").await.unwrap().ref_count, 1, "blobA ref");

            // Replacing the artifact's blob moves the reference.
            s.put_artifact(Artifact {
                repo_id: repo.id,
                path: "a/1.0/a-1.0.tgz".into(),
                version: "1.0".into(),
                blob_sha256: "blobB".into(),
                size: 12,
                ..Artifact::default()
            })
            .await
            .unwrap();
            assert_eq!(s.get_blob("blobA").await.unwrap().ref_count, 0, "blobA ref");
            assert_eq!(s.get_blob("blobB").await.unwrap().ref_count, 1, "blobB ref");

            let unref = s.list_unreferenced_blobs(10, Utc::now()).await.unwrap();
            assert_eq!(unref, vec!["blobA".to_string()]);

            // Deleting the artifact releases blobB.
            s.delete_artifact(repo.id, "a/1.0/a-1.0.tgz").await.unwrap();
            assert_eq!(s.get_blob("blobB").await.unwrap().ref_count, 0, "blobB ref");
        }

        #[tokio::test]
        async fn artifact_list_and_evict() {
            let (s, _dir) = crate::testing::meta::test_store().await;
            let repo = s
                .create_repository(Repository {
                    name: "r".into(),
                    format: FORMAT_GO.into(),
                    r#type: TYPE_PROXY.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();

            for p in ["x/v1", "x/v2", "y/v1"] {
                s.put_artifact(Artifact {
                    repo_id: repo.id,
                    path: p.into(),
                    blob_sha256: format!("blob-{p}"),
                    size: 100,
                    ..Artifact::default()
                })
                .await
                .unwrap();
            }
            let xs = s.list_artifacts(repo.id, "x/").await.expect("list x/");
            assert_eq!(xs.len(), 2);

            assert_eq!(s.repo_size(repo.id).await.unwrap(), 300, "repo size");

            // Touch x/v1 so it is most-recently used, then evict one (the oldest).
            s.touch(repo.id, "x/v1", "tester").await.unwrap();
            let n = s.evict_lru(repo.id, 1).await.expect("evict");
            assert_eq!(n, 1);
            let remaining = s.list_artifacts(repo.id, "").await.unwrap();
            assert_eq!(remaining.len(), 2, "after evict");
        }

        #[tokio::test]
        async fn get_artifact_not_found() {
            let (s, _dir) = crate::testing::meta::test_store().await;
            let err = s.get_artifact(1, "nope").await.unwrap_err();
            assert!(err.is_not_found(), "want NotFound, got {err}");
        }

        #[tokio::test]
        async fn list_repo_artifacts_and_count() {
            let (s, _dir) = crate::testing::meta::test_store().await;
            let repo = s
                .create_repository(Repository {
                    name: "r".into(),
                    format: FORMAT_NPM.into(),
                    r#type: TYPE_PROXY.into(),
                    ..Repository::default()
                })
                .await
                .unwrap();
            for p in ["a/1", "a/2", "b/1"] {
                s.put_artifact(Artifact {
                    repo_id: repo.id,
                    path: p.into(),
                    blob_sha256: format!("b{p}"),
                    size: 50,
                    ..Artifact::default()
                })
                .await
                .unwrap();
            }
            let all = s
                .list_repo_artifacts(repo.id, "", 0)
                .await
                .expect("list all");
            assert_eq!(all.len(), 3);
            let as_ = s.list_repo_artifacts(repo.id, "a/", 10).await.unwrap();
            assert_eq!(as_.len(), 2, "prefix a/");
            assert_eq!(s.count_artifacts(repo.id).await.unwrap(), 3, "count");
        }
    }
}
