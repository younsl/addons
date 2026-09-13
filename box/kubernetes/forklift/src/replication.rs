//! PV-based active/standby replication. Each replica keeps its own
//! (ReadWriteOnce) PersistentVolume; the standby continuously pulls the
//! leader's SQLite snapshot and content-addressed blobs over token-
//! authenticated internal HTTP endpoints, then promotes that copy when it
//! acquires leadership. This removes the ReadWriteMany storage requirement of
//! the shared-volume HA mode at the cost of asynchronous replication: writes
//! within one pull interval can be lost on failover.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use futures_util::TryStreamExt;
use prometheus::{Counter, CounterVec, Gauge, Opts, Registry};
use tokio::sync::Mutex;
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;

use crate::{meta, storage};

mod source;

pub use source::Source;
use source::{BlobPage, DEFAULT_PAGE_SIZE};

/// Result alias used throughout the module.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised by the standby pull loop.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A message with no cause, e.g. `fetch snapshot: status 503`.
    #[error("{0}")]
    Message(String),
    #[error("{context}: {source}")]
    Wrapped {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    /// A transport failure talking to the leader.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl Error {
    /// Builds a bare message error.
    pub fn msg(msg: impl Into<String>) -> Self {
        Error::Message(msg.into())
    }

    pub fn wrap<E>(context: impl Into<String>, source: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Error::Wrapped {
            context: context.into(),
            source: source.into(),
        }
    }
}

/// Returns the base URL of the current leader, or `""` when the leader is unknown or is this
/// instance (in which case the standby skips the sync cycle).
#[async_trait]
pub trait LeaderResolver: Send + Sync {
    /// Resolves the current leader's base URL.
    async fn leader_url(&self) -> anyhow::Result<String>;
}

/// Implemented by `cluster::Elector`.
#[async_trait]
pub trait LeaderLookup: Send + Sync {
    /// Returns the identity currently holding the Lease, or `""` when the
    /// Lease is unheld.
    async fn leader_identity(&self) -> anyhow::Result<String>;
}

/// Resolves to a fixed URL (testing / non-Kubernetes setups). Bridges the Kubernetes Lease
/// elector to [`LeaderLookup`] so [`lease_leader_url`] can address the pod that currently holds
/// the Lease.
#[async_trait]
impl LeaderLookup for crate::cluster::Elector {
    async fn leader_identity(&self) -> anyhow::Result<String> {
        Ok(crate::cluster::Elector::leader_identity(self).await?)
    }
}

pub fn static_leader_url(url: &str) -> Arc<dyn LeaderResolver> {
    struct Static(String);
    #[async_trait]
    impl LeaderResolver for Static {
        async fn leader_url(&self) -> anyhow::Result<String> {
            Ok(self.0.clone())
        }
    }
    Arc::new(Static(url.to_string()))
}

/// Resolves the leader pod through the Lease holder identity and the headless
/// Service domain: `http://<holder>.<peer_service>:<peer_port>`. It returns
/// `""` when this instance holds the Lease.
pub fn lease_leader_url(
    elector: Arc<dyn LeaderLookup>,
    identity: &str,
    peer_service: &str,
    peer_port: i64,
) -> Arc<dyn LeaderResolver> {
    struct Lease {
        elector: Arc<dyn LeaderLookup>,
        identity: String,
        peer_service: String,
        peer_port: i64,
    }
    #[async_trait]
    impl LeaderResolver for Lease {
        async fn leader_url(&self) -> anyhow::Result<String> {
            let id = self.elector.leader_identity().await?;
            if id.is_empty() || id == self.identity {
                return Ok(String::new());
            }
            Ok(format!(
                "http://{}.{}:{}",
                id, self.peer_service, self.peer_port
            ))
        }
    }
    Arc::new(Lease {
        elector,
        identity: identity.to_string(),
        peer_service: peer_service.to_string(),
        peer_port,
    })
}

/// Configures a [`Replicator`].
pub struct Options {
    pub store: Arc<meta::Store>,
    pub blobs: Arc<dyn storage::WalkableStore>,
    pub data_dir: PathBuf,
    pub token: String,
    pub interval: Duration,
    pub leader_url: Arc<dyn LeaderResolver>,
    pub registry: Option<Registry>,
}

/// The standby-side pull loop. Every interval, while this instance is not the
/// leader, it downloads a fresh SQLite snapshot, mirrors the leader's blob set
/// onto the local volume, and only then commits the snapshot, so a committed
/// snapshot never references blobs that were not mirrored. [`Replicator::promote`]
/// applies the latest committed snapshot when leadership is acquired.
pub struct Replicator {
    store: Arc<meta::Store>,
    blobs: Arc<dyn storage::WalkableStore>,
    data_dir: PathBuf,
    token: String,
    interval: Duration,
    leader_url: Arc<dyn LeaderResolver>,
    client: reqwest::Client,

    is_leader: AtomicBool,

    /// Serializes sync cycles with promotion so `promote` never races a
    /// half-written snapshot. It guards the path of the last snapshot fully
    /// downloaded during this process's standby phase; `None` when there is
    /// none. Stale on-disk snapshots from previous runs are deliberately
    /// ignored (and removed at startup): applying one on a pod that was
    /// recently the leader would roll back its newer local data.
    sync_mu: Mutex<Option<PathBuf>>,

    syncs: CounterVec,
    blobs_fetched: Counter,
    blobs_deleted: Counter,
    last_sync_unix: Gauge,
    snapshot_bytes: Gauge,
}

/// One step of the ordered local/remote digest merge.
enum MergeStep {
    Fetch(String),
    Delete(String),
    Skip,
    Done,
}

impl Replicator {
    /// Builds a `Replicator` and registers its metrics.
    pub fn new(o: Options) -> Arc<Replicator> {
        let syncs = CounterVec::new(
            Opts::new(
                "replication_syncs_total",
                "Replication sync cycles by result.",
            )
            .namespace("forklift"),
            &["result"],
        )
        .expect("replication_syncs_total definition is static and valid");
        let blobs_fetched = Counter::with_opts(
            Opts::new(
                "replication_blobs_fetched_total",
                "Blobs downloaded from the leader.",
            )
            .namespace("forklift"),
        )
        .expect("replication_blobs_fetched_total definition is static and valid");
        let blobs_deleted = Counter::with_opts(
            Opts::new(
                "replication_blobs_deleted_total",
                "Local blobs deleted because the leader no longer has them.",
            )
            .namespace("forklift"),
        )
        .expect("replication_blobs_deleted_total definition is static and valid");
        let last_sync_unix = Gauge::with_opts(
            Opts::new(
                "replication_last_sync_timestamp_seconds",
                "Unix time of the last successful sync cycle.",
            )
            .namespace("forklift"),
        )
        .expect("replication_last_sync_timestamp_seconds definition is static and valid");
        let snapshot_bytes = Gauge::with_opts(
            Opts::new(
                "replication_snapshot_bytes",
                "Size of the last downloaded database snapshot.",
            )
            .namespace("forklift"),
        )
        .expect("replication_snapshot_bytes definition is static and valid");

        if let Some(reg) = &o.registry {
            reg.register(Box::new(syncs.clone()))
                .expect("register forklift_replication_syncs_total");
            reg.register(Box::new(blobs_fetched.clone()))
                .expect("register forklift_replication_blobs_fetched_total");
            reg.register(Box::new(blobs_deleted.clone()))
                .expect("register forklift_replication_blobs_deleted_total");
            reg.register(Box::new(last_sync_unix.clone()))
                .expect("register forklift_replication_last_sync_timestamp_seconds");
            reg.register(Box::new(snapshot_bytes.clone()))
                .expect("register forklift_replication_snapshot_bytes");
        }

        Arc::new(Replicator {
            store: o.store,
            blobs: o.blobs,
            data_dir: o.data_dir,
            token: o.token,
            interval: o.interval,
            leader_url: o.leader_url,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5 * 60))
                .build()
                .expect("build replication http client"),
            is_leader: AtomicBool::new(false),
            sync_mu: Mutex::new(None),
            syncs,
            blobs_fetched,
            blobs_deleted,
            last_sync_unix,
            snapshot_bytes,
        })
    }

    fn replica_dir(&self) -> PathBuf {
        self.data_dir.join("replica")
    }

    /// Executes the pull loop until `cancel` fires. Stale snapshots from
    /// previous runs are removed first so [`Replicator::promote`] only ever
    /// applies data pulled during this process's standby phase.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        match tokio::fs::remove_dir_all(self.replica_dir()).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(err = %e, "replication: clean stale replica dir"),
        }
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `time.NewTicker` does not fire immediately; tokio's interval does.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    if self.is_leader.load(Ordering::SeqCst) {
                        continue;
                    }
                    if let Err(e) = self.sync().await
                        && !cancel.is_cancelled()
                    {
                        self.syncs.with_label_values(&["error"]).inc();
                        tracing::error!(err = %e, "replication: sync failed");
                    }
                }
            }
        }
    }

    /// Runs one pull cycle: snapshot first, blobs second, commit last.
    ///
    /// The blob listing reflects the leader's live database, which is at least
    /// as new as the snapshot, so every blob a committed snapshot references
    /// has been mirrored before `promote` can apply it. Committing before the
    /// blob sync would let a failover serve metadata whose blobs were never
    /// fetched.
    async fn sync(&self) -> Result<()> {
        let mut committed = self.sync_mu.lock().await;
        if self.is_leader.load(Ordering::SeqCst) {
            return Ok(());
        }
        let leader = self
            .leader_url
            .leader_url()
            .await
            .map_err(|e| Error::wrap("resolve leader", e))?;
        if leader.is_empty() {
            return Ok(());
        }
        let (tmp, size) = self
            .download_snapshot(&leader)
            .await
            .map_err(|e| Error::wrap("sync db", e))?;
        if let Err(e) = self.sync_blobs(&leader).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::wrap("sync blobs", e));
        }
        let final_path = self.replica_dir().join("forklift.db");
        if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::wrap("commit snapshot", e));
        }
        *committed = Some(final_path);
        self.snapshot_bytes.set(size as f64);
        self.syncs.with_label_values(&["ok"]).inc();
        self.last_sync_unix.set(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or_default(),
        );
        Ok(())
    }

    /// Mirrors the leader's blob set: both sides enumerate digests in
    /// lexicographic order, so a streaming merge finds missing and extra blobs
    /// without paging either set repeatedly. Blobs are immutable and
    /// content-addressed, which makes fetch and delete both idempotent.
    async fn sync_blobs(&self, leader: &str) -> Result<()> {
        // `WalkDigests` is a push-style callback here and Rust has no stable coroutine to
        // invert it, so the local digest list is materialised (64 bytes per blob) while the
        // leader's side still streams a page at a time.
        let mut local = Vec::new();
        self.blobs
            .walk_digests(&mut |d: &str| {
                local.push(d.to_string());
                Ok(())
            })
            .await
            .map_err(|e| Error::wrap("walk local blobs", e))?;

        let mut locals = local.into_iter();
        let mut pages = RemotePages::default();
        let mut l = locals.next();
        let mut rd = pages.next(self, leader).await?;
        loop {
            let step = match (l.as_deref(), rd.as_deref()) {
                (None, None) => MergeStep::Done,
                (None, Some(r)) => MergeStep::Fetch(r.to_string()),
                (Some(lv), None) => MergeStep::Delete(lv.to_string()),
                (Some(lv), Some(rv)) if rv < lv => MergeStep::Fetch(rv.to_string()),
                (Some(lv), Some(rv)) if lv < rv => MergeStep::Delete(lv.to_string()),
                (Some(_), Some(_)) => MergeStep::Skip,
            };
            match step {
                MergeStep::Done => return Ok(()),
                MergeStep::Fetch(d) => {
                    self.fetch_blob(leader, &d).await?;
                    rd = pages.next(self, leader).await?;
                }
                MergeStep::Delete(d) => {
                    self.blobs
                        .delete(&d)
                        .await
                        .map_err(|e| Error::wrap(format!("delete extra blob {d}"), e))?;
                    self.blobs_deleted.inc();
                    l = locals.next();
                }
                MergeStep::Skip => {
                    l = locals.next();
                    rd = pages.next(self, leader).await?;
                }
            }
        }
    }

    async fn fetch_blob_page(&self, leader: &str, after: &str) -> Result<Vec<String>> {
        let escaped: String = form_urlencoded::byte_serialize(after.as_bytes()).collect();
        let u = format!(
            "{leader}/internal/replication/blobs?limit={DEFAULT_PAGE_SIZE}&after={escaped}"
        );
        let resp = self.get(&u).await?;
        if resp.status() != StatusCode::OK {
            return Err(Error::msg(format!(
                "list blobs: status {}",
                resp.status().as_u16()
            )));
        }
        let page: BlobPage = resp
            .json()
            .await
            .map_err(|e| Error::wrap("decode blob page", e))?;
        Ok(page.digests)
    }

    async fn fetch_blob(&self, leader: &str, digest: &str) -> Result<()> {
        let resp = self
            .get(&format!("{leader}/internal/replication/blobs/{digest}"))
            .await?;
        // The leader may have garbage-collected the blob since listing it.
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if resp.status() != StatusCode::OK {
            return Err(Error::msg(format!(
                "fetch blob {digest}: status {}",
                resp.status().as_u16()
            )));
        }
        let body = StreamReader::new(resp.bytes_stream().map_err(std::io::Error::other));
        let (got, _) = self
            .blobs
            .put(Box::pin(body))
            .await
            .map_err(|e| Error::wrap(format!("store blob {digest}"), e))?;
        // Put re-hashes the stream, so a digest mismatch means corruption in flight.
        if got != digest {
            let _ = self.blobs.delete(&got).await;
            return Err(Error::msg(format!(
                "blob digest mismatch: want {digest} got {got}"
            )));
        }
        self.blobs_fetched.inc();
        Ok(())
    }

    /// Fetches a fresh snapshot into a temp file and returns its path and size.
    /// The caller commits it with an atomic rename only after the blob sync
    /// succeeds, so `promote` never sees a partial or blob-incomplete snapshot.
    async fn download_snapshot(&self, leader: &str) -> Result<(PathBuf, i64)> {
        tokio::fs::create_dir_all(self.replica_dir())
            .await
            .map_err(|e| Error::wrap("create replica dir", e))?;
        let resp = self
            .get(&format!("{leader}/internal/replication/db"))
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(Error::msg(format!(
                "fetch snapshot: status {}",
                resp.status().as_u16()
            )));
        }

        let tmp = self.replica_dir().join("forklift.db.tmp");
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| Error::wrap("create snapshot file", e))?;
        let mut body = StreamReader::new(resp.bytes_stream().map_err(std::io::Error::other));
        let n = match tokio::io::copy(&mut body, &mut f).await {
            Ok(n) => n as i64,
            Err(e) => {
                drop(f);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(Error::wrap("download snapshot", e));
            }
        };
        if let Err(e) = f.sync_all().await {
            drop(f);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::wrap("sync snapshot", e));
        }
        Ok((tmp, n))
    }

    async fn get(&self, u: &str) -> Result<reqwest::Response> {
        Ok(self
            .client
            .get(u)
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .send()
            .await?)
    }

    /// Called when this instance acquires leadership, before it reports Ready.
    /// If a snapshot was replicated during the standby phase it replaces the
    /// local database; otherwise the local data is served as-is (first start,
    /// or a re-elected former leader).
    pub async fn promote(&self) -> Result<()> {
        self.is_leader.store(true, Ordering::SeqCst);
        let mut committed = self.sync_mu.lock().await;
        let Some(path) = committed.take() else {
            tracing::info!("replication: promoting with local data (no replicated snapshot)");
            return Ok(());
        };
        self.store
            .swap_from_snapshot(&path)
            .await
            .map_err(|e| Error::wrap("apply replicated snapshot", e))?;
        tracing::info!("replication: promoted with replicated snapshot");
        Ok(())
    }

    /// Called when leadership is lost; the pull loop resumes.
    pub fn demote(&self) {
        self.is_leader.store(false, Ordering::SeqCst);
    }
}

/// Cursor-paged view of the leader's digest listing. Pages are pulled only as
/// the merge consumes them, so the standby never holds more than one page.
#[derive(Default)]
struct RemotePages {
    buf: std::vec::IntoIter<String>,
    after: String,
    done: bool,
}

impl RemotePages {
    async fn next(&mut self, r: &Replicator, leader: &str) -> Result<Option<String>> {
        loop {
            if let Some(d) = self.buf.next() {
                self.after = d.clone();
                return Ok(Some(d));
            }
            if self.done {
                return Ok(None);
            }
            let page = r
                .fetch_blob_page(leader, &self.after)
                .await
                .map_err(|e| Error::wrap("list leader blobs", e))?;
            if page.is_empty() {
                self.done = true;
                return Ok(None);
            }
            self.buf = page.into_iter();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rusqlite::params;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    use crate::meta::{FORMAT_MAVEN, FORMAT_NPM, Repository, TYPE_HOSTED, TYPE_PROXY};
    use crate::replication::*;
    use crate::storage::{BlobStore, FsStore, WalkableStore};

    const TEST_TOKEN: &str = "test-replication-token";

    /// reqwest builds a rustls client eagerly; the process-wide provider is
    /// installed by the server binary in production.
    fn install_crypto_provider() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    struct TestServer {
        url: String,
        handle: JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn serve_router(router: axum::Router) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        TestServer { url, handle }
    }

    struct Env {
        dir: tempfile::TempDir,
        store: Arc<meta::Store>,
        blobs: Arc<FsStore>,
    }

    async fn new_env() -> Env {
        install_crypto_provider();
        let dir = tempfile::tempdir().expect("temp dir");
        let store = meta::Store::open(dir.path().join("forklift.db"))
            .await
            .expect("open store");
        let blobs = FsStore::new(dir.path()).expect("open blobs");
        Env {
            dir,
            store: Arc::new(store),
            blobs: Arc::new(blobs),
        }
    }

    impl Env {
        /// Stores content and records the blob row so listings see it.
        async fn put_blob(&self, content: &str) -> String {
            let (digest, size) = self
                .blobs
                .put(Box::pin(std::io::Cursor::new(content.as_bytes().to_vec())))
                .await
                .expect("put blob");
            let created = meta::now_rfc3339();
            let d = digest.clone();
            self.store
            .write(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO blobs(sha256, size, ref_count, created_at) VALUES(?, ?, 1, ?)",
                    params![d, size, created],
                )
                .map_err(|e| meta::Error::sqlite("record blob", e))?;
                Ok(())
            })
            .await
            .expect("record blob");
            digest
        }

        fn source(&self) -> Arc<Source> {
            Source::new(
                Arc::clone(&self.store),
                Arc::clone(&self.blobs) as Arc<dyn BlobStore>,
                TEST_TOKEN,
                self.dir.path(),
            )
        }

        fn mounted(&self) -> axum::Router {
            axum::Router::new().nest("/internal/replication", self.source().routes())
        }

        async fn serve(&self) -> TestServer {
            serve_router(self.mounted()).await
        }

        fn replicator(&self, leader_url: &str) -> Arc<Replicator> {
            Replicator::new(Options {
                store: Arc::clone(&self.store),
                blobs: Arc::clone(&self.blobs) as Arc<dyn WalkableStore>,
                data_dir: self.dir.path().to_path_buf(),
                token: TEST_TOKEN.to_string(),
                interval: Duration::from_millis(10),
                leader_url: static_leader_url(leader_url),
                registry: Some(Registry::new()),
            })
        }
    }

    async fn local_digests(blobs: &FsStore) -> Vec<String> {
        let mut out = Vec::new();
        blobs
            .walk_digests(&mut |d: &str| {
                out.push(d.to_string());
                Ok(())
            })
            .await
            .expect("walk");
        out
    }

    fn client() -> reqwest::Client {
        install_crypto_provider();
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn source_requires_token() {
        let leader = new_env().await;
        let ts = leader.serve().await;
        let c = client();

        for auth in ["", "Bearer wrong"] {
            let mut req = c.get(format!("{}/internal/replication/blobs", ts.url));
            if !auth.is_empty() {
                req = req.header("Authorization", auth);
            }
            let resp = req.send().await.expect("request");
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "auth {auth:?}: want 401"
            );
        }

        let resp = c
            .get(format!("{}/internal/replication/blobs", ts.url))
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn source_list_blobs_paging() {
        let leader = new_env().await;
        let mut want = std::collections::HashSet::new();
        for i in 0..3 {
            want.insert(leader.put_blob(&format!("content-{i}")).await);
        }
        let ts = leader.serve().await;
        let c = client();

        let mut got: Vec<String> = Vec::new();
        let mut after = String::new();
        loop {
            let resp = c
                .get(format!(
                    "{}/internal/replication/blobs?limit=2&after={after}",
                    ts.url
                ))
                .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                .send()
                .await
                .expect("request");
            let page: BlobPage = resp.json().await.expect("decode page");
            if page.digests.is_empty() {
                break;
            }
            after = page.digests[page.digests.len() - 1].clone();
            got.extend(page.digests);
        }
        assert_eq!(got.len(), want.len(), "digest count");
        for i in 1..got.len() {
            assert!(got[i - 1] < got[i], "digests not strictly ordered: {got:?}");
        }
        for d in &got {
            assert!(want.contains(d), "unexpected digest {d}");
        }
    }

    #[tokio::test]
    async fn source_list_blobs_rejects_bad_limit() {
        let leader = new_env().await;
        let ts = leader.serve().await;
        let c = client();
        for limit in ["0", "-1", "9999", "abc"] {
            let resp = c
                .get(format!(
                    "{}/internal/replication/blobs?limit={limit}",
                    ts.url
                ))
                .header("Authorization", format!("Bearer {TEST_TOKEN}"))
                .send()
                .await
                .expect("request");
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "limit {limit:?}: want 400"
            );
        }
    }

    #[tokio::test]
    async fn source_get_blob_not_found() {
        let leader = new_env().await;
        let ts = leader.serve().await;
        let missing = "0".repeat(64);
        let resp = client()
            .get(format!("{}/internal/replication/blobs/{missing}", ts.url))
            .header("Authorization", format!("Bearer {TEST_TOKEN}"))
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sync_and_promote() {
        let leader = new_env().await;
        let d1 = leader.put_blob("blob-one").await;
        let d2 = leader.put_blob("blob-two").await;
        let repo = leader
            .store
            .create_repository(Repository {
                name: "npm-proxy".into(),
                format: FORMAT_NPM.into(),
                r#type: TYPE_PROXY.into(),
                upstream_url: "https://registry.npmjs.org".into(),
                ..Repository::default()
            })
            .await
            .expect("create repo");
        let ts = leader.serve().await;

        let standby = new_env().await;
        let extra = standby.put_blob("standby-only-blob").await;
        let rep = standby.replicator(&ts.url);

        rep.sync().await.expect("sync");

        let got = local_digests(&standby.blobs).await;
        assert_eq!(
            got.len(),
            2,
            "standby has {} blobs {got:?}, want 2",
            got.len()
        );
        for d in &got {
            assert!(
                *d == d1 || *d == d2,
                "unexpected standby blob {d} (extra {extra} should be deleted)"
            );
        }

        // Round-trip the bytes to prove content integrity, not just presence.
        let (mut rc, _) = standby.blobs.open(&d1).await.expect("open synced blob");
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut rc, &mut buf)
            .await
            .expect("read synced blob");
        assert_eq!(String::from_utf8_lossy(&buf), "blob-one");

        // Promotion applies the replicated snapshot: the leader's repository becomes
        // visible through the standby's store handle.
        assert!(
            standby
                .store
                .get_repository_by_name("npm-proxy")
                .await
                .is_err(),
            "standby unexpectedly has leader repo before promote"
        );
        rep.promote().await.expect("promote");
        let promoted = standby
            .store
            .get_repository_by_name("npm-proxy")
            .await
            .expect("repo after promote");
        assert_eq!(promoted.id, repo.id, "promoted repo ID");

        // The consumed snapshot must not be re-applied on a later promotion.
        rep.promote().await.expect("second promote");
    }

    #[tokio::test]
    async fn promote_without_snapshot_keeps_local_data() {
        let e = new_env().await;
        e.store
            .create_repository(Repository {
                name: "local-maven".into(),
                format: FORMAT_MAVEN.into(),
                r#type: TYPE_HOSTED.into(),
                ..Repository::default()
            })
            .await
            .expect("create repo");
        let rep = e.replicator("");
        rep.promote().await.expect("promote");
        e.store
            .get_repository_by_name("local-maven")
            .await
            .expect("local data lost on promote");
    }

    /// Pins the commit ordering: a snapshot must only become visible to `promote`
    /// after the blob mirror it references has been pulled. Otherwise a failover
    /// right after a sync could serve metadata whose blobs were never fetched.
    #[tokio::test]
    async fn sync_does_not_commit_snapshot_when_blob_sync_fails() {
        let leader = new_env().await;
        // Real /db endpoint, failing /blobs listing.
        let router = axum::Router::new()
            .route(
                "/internal/replication/blobs",
                axum::routing::get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [(
                            axum::http::header::CONTENT_TYPE,
                            "text/plain; charset=utf-8",
                        )],
                        "boom\n",
                    )
                }),
            )
            .fallback_service(leader.mounted());
        let ts = serve_router(router).await;

        let standby = new_env().await;
        let rep = standby.replicator(&ts.url);
        assert!(
            rep.sync().await.is_err(),
            "expected sync error from failing blob listing"
        );
        assert!(
            rep.sync_mu.lock().await.is_none(),
            "snapshot committed despite failed blob sync"
        );
        assert!(
            !standby
                .dir
                .path()
                .join("replica")
                .join("forklift.db")
                .exists(),
            "snapshot file should not exist after failed blob sync"
        );
        assert!(
            !standby
                .dir
                .path()
                .join("replica")
                .join("forklift.db.tmp")
                .exists(),
            "temp snapshot should be removed after failed blob sync"
        );
    }

    #[tokio::test]
    async fn sync_skips_when_leader_unknown_or_self() {
        let e = new_env().await;
        let rep = e.replicator(""); // resolver returns ""
        rep.sync().await.expect("sync should skip");
        rep.is_leader.store(true, Ordering::SeqCst);
        rep.sync().await.expect("sync as leader should skip");
    }

    #[tokio::test]
    async fn fetch_blob_digest_mismatch() {
        let router = axum::Router::new().route(
            "/internal/replication/blobs/{digest}",
            axum::routing::get(|| async { "not the promised content" }),
        );
        let ts = serve_router(router).await;

        let e = new_env().await;
        let rep = e.replicator(&ts.url);
        let want_digest = "1".repeat(64);
        assert!(
            rep.fetch_blob(&ts.url, &want_digest).await.is_err(),
            "expected digest mismatch error"
        );
        let got = local_digests(&e.blobs).await;
        assert!(got.is_empty(), "mismatched blob must not be kept: {got:?}");
    }

    #[tokio::test]
    async fn run_cleans_stale_replica_dir() {
        let e = new_env().await;
        let stale = e.dir.path().join("replica").join("forklift.db");
        tokio::fs::create_dir_all(stale.parent().expect("parent"))
            .await
            .expect("mkdir");
        tokio::fs::write(&stale, b"stale").await.expect("write");

        let rep = e.replicator("");
        let cancel = CancellationToken::new();
        let running = tokio::spawn(Arc::clone(&rep).run(cancel.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        running.await.expect("run");

        assert!(
            !stale.exists(),
            "stale replica snapshot should be removed at startup"
        );
    }

    struct FakeHolder {
        id: String,
    }

    #[async_trait]
    impl LeaderLookup for FakeHolder {
        async fn leader_identity(&self) -> anyhow::Result<String> {
            Ok(self.id.clone())
        }
    }

    fn holder(id: &str) -> Arc<dyn LeaderLookup> {
        Arc::new(FakeHolder { id: id.to_string() })
    }

    #[tokio::test]
    async fn lease_leader_url_resolves() {
        let resolve = lease_leader_url(
            holder("forklift-0"),
            "forklift-1",
            "forklift-headless.tools.svc",
            8080,
        );
        let u = resolve.leader_url().await.expect("resolve");
        assert_eq!(u, "http://forklift-0.forklift-headless.tools.svc:8080");

        // Self holds the lease: no leader to pull from.
        let resolve = lease_leader_url(holder("forklift-1"), "forklift-1", "svc", 8080);
        assert_eq!(
            resolve.leader_url().await.unwrap_or_default(),
            "",
            "self leader should resolve to empty"
        );

        // No holder yet.
        let resolve = lease_leader_url(holder(""), "forklift-1", "svc", 8080);
        assert_eq!(
            resolve.leader_url().await.unwrap_or_default(),
            "",
            "no holder should resolve to empty"
        );
    }
}
