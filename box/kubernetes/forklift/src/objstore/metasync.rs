//! The metadata snapshot sync itself: the object API abstraction, the leader's
//! upload path with its fencing rules, and the standby's download path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_runtime_api::client::result::SdkError;
use prometheus::{Gauge, IntCounterVec, Opts, Registry};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::meta;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The object does not exist (S3 `NoSuchKey`/`NotFound`/404).
    #[error("not found")]
    NotFound,
    /// A conditional write was rejected (S3 412), i.e. another writer changed
    /// the object first.
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    /// Any other object-store failure.
    #[error("{0}")]
    ObjectStore(String),
    /// A metadata store failure (snapshot, swap).
    #[error("{0}")]
    Meta(#[from] meta::Error),
    /// A filesystem failure.
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Fencing(String),
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    fn io(op: &'static str, source: std::io::Error) -> Error {
        Error::Io { op, source }
    }

    fn context(self, context: impl Into<String>) -> Error {
        Error::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }

    pub fn is_not_found(&self) -> bool {
        match self {
            Error::NotFound => true,
            Error::Context { source, .. } => source.is_not_found(),
            _ => false,
        }
    }

    /// Reports whether this is (or wraps) an S3 412 Precondition Failed.
    pub fn is_precondition_failed(&self) -> bool {
        match self {
            Error::PreconditionFailed(_) => true,
            Error::Context { source, .. } => source.is_precondition_failed(),
            _ => false,
        }
    }
}

/// Result alias used throughout the module.
pub type Result<T> = std::result::Result<T, Error>;

/// The body of a [`PutObjectInput`].
#[derive(Debug, Clone)]
pub enum PutBody {
    /// Stream the file at this path.
    File(PathBuf),
    /// Upload these bytes.
    Bytes(Vec<u8>),
}

/// The fields of `s3.PutObjectInput` this module sets.
#[derive(Debug, Clone)]
pub struct PutObjectInput {
    pub bucket: String,
    pub key: String,
    pub body: PutBody,
    pub content_length: i64,
    /// S3 user-metadata; the fencing token lives here.
    pub metadata: HashMap<String, String>,
    /// Conditional write against the observed ETag.
    pub if_match: Option<String>,
    /// Conditional write for a first put (`*`).
    pub if_none_match: Option<String>,
}

/// The fields of `s3.PutObjectOutput` this module reads.
#[derive(Debug, Clone, Default)]
pub struct PutObjectOutput {
    pub e_tag: Option<String>,
}

/// The fields of `s3.GetObjectOutput` this module reads.
pub struct GetObjectOutput {
    pub body: Box<dyn AsyncRead + Send + Unpin>,
    pub content_length: Option<i64>,
    pub e_tag: Option<String>,
}

/// The fields of `s3.HeadObjectOutput` this module reads.
#[derive(Debug, Clone, Default)]
pub struct HeadObjectOutput {
    pub e_tag: Option<String>,
    pub metadata: HashMap<String, String>,
    pub content_length: Option<i64>,
}

/// The object operations this module performs.
#[async_trait]
pub trait ObjectApi: Send + Sync {
    async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput>;
    async fn get_object(&self, bucket: &str, key: &str) -> Result<GetObjectOutput>;
    async fn head_object(&self, bucket: &str, key: &str) -> Result<HeadObjectOutput>;
    async fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
}

/// [`ObjectApi`] over the AWS SDK's S3 client, the one production
/// implementation. Build it from the client `storage::S3BlobStore::client()`
/// hands the caller:
///
/// ```no_run
/// use std::sync::Arc;
/// use forklift::objstore::{ObjectApi, S3Api};
/// # fn example(client: aws_sdk_s3::Client) {
/// let api: Arc<dyn ObjectApi> = Arc::new(S3Api::new(client));
/// # }
/// ```
pub struct S3Api(pub aws_sdk_s3::Client);

impl S3Api {
    /// Wraps an S3 client.
    pub fn new(client: aws_sdk_s3::Client) -> S3Api {
        S3Api(client)
    }
}

/// Reports whether `err` is an S3 "no such key"/404, across the SDK's typed
/// errors and the generic HTTP response error.
fn is_not_found<E: ProvideErrorMetadata>(err: &SdkError<E, HttpResponse>) -> bool {
    if matches!(err.code(), Some("NoSuchKey" | "NotFound")) {
        return true;
    }
    err.raw_response().map(|r| r.status().as_u16()) == Some(404)
}

/// Reports whether `err` is an S3 412 Precondition Failed, returned when a
/// conditional write (If-Match / If-None-Match) is rejected because another
/// writer changed the object first. Fencing treats it as "lost the race, skip
/// this cycle".
///
fn is_precondition_failed<E>(err: &SdkError<E, HttpResponse>) -> bool {
    err.raw_response().map(|r| r.status().as_u16()) == Some(412)
}

fn sdk_error<E>(op: &'static str, err: SdkError<E, HttpResponse>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    if is_not_found(&err) {
        return Error::NotFound;
    }
    if is_precondition_failed(&err) {
        return Error::PreconditionFailed(format!("{op}: {err}"));
    }
    Error::ObjectStore(format!("{op}: {err}"))
}

#[async_trait]
impl ObjectApi for S3Api {
    async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput> {
        let body = match &input.body {
            PutBody::File(path) => aws_sdk_s3::primitives::ByteStream::from_path(path)
                .await
                .map_err(|e| Error::ObjectStore(e.to_string()))?,
            PutBody::Bytes(b) => aws_sdk_s3::primitives::ByteStream::from(b.clone()),
        };
        let mut req = self
            .0
            .put_object()
            .bucket(&input.bucket)
            .key(&input.key)
            .body(body)
            .content_length(input.content_length)
            .set_metadata(Some(input.metadata.clone()));
        if let Some(v) = &input.if_match {
            req = req.if_match(v);
        }
        if let Some(v) = &input.if_none_match {
            req = req.if_none_match(v);
        }
        let out = req.send().await.map_err(|e| sdk_error("put object", e))?;
        Ok(PutObjectOutput {
            e_tag: out.e_tag().map(str::to_owned),
        })
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<GetObjectOutput> {
        let out = self
            .0
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_error("get object", e))?;
        let content_length = out.content_length();
        let e_tag = out.e_tag().map(str::to_owned);
        Ok(GetObjectOutput {
            body: Box::new(out.body.into_async_read()),
            content_length,
            e_tag,
        })
    }

    async fn head_object(&self, bucket: &str, key: &str) -> Result<HeadObjectOutput> {
        let out = self
            .0
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_error("head object", e))?;
        Ok(HeadObjectOutput {
            e_tag: out.e_tag().map(str::to_owned),
            metadata: out.metadata().cloned().unwrap_or_default(),
            content_length: out.content_length(),
        })
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.0
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| sdk_error("delete object", e))
    }
}

/// Configures a [`MetaSync`].
pub struct MetaOptions {
    pub store: Arc<meta::Store>,
    pub api: Arc<dyn ObjectApi>,
    pub bucket: String,
    /// Object key for the snapshot, e.g. `"<prefix>/meta/forklift.db"`.
    pub key: String,
    pub data_dir: PathBuf,
    pub interval: Duration,
    pub registry: Option<Registry>,
}

/// The state guarded by `sync_mu`: the standby's downloaded copy and the
/// leader's change-detection baseline.
#[derive(Default)]
struct SyncState {
    /// The last snapshot downloaded during this process's standby phase; `None`
    /// when none. `promote` applies it and clears it.
    snapshot_path: Option<PathBuf>,
    /// Identifies the S3 object `snapshot_path` was downloaded from, so
    /// `promote` can tell whether that copy is still the current one, and the
    /// standby loop can skip re-downloading an object it already holds.
    snapshot_etag: Option<String>,
    /// Detects commits since the last upload so an idle leader does not VACUUM
    /// and upload an identical snapshot every interval: for a database in the
    /// gigabyte range that is the whole of the pod's page-cache churn and
    /// network egress. Held only while leader; `None` otherwise.
    watch: Option<meta::ChangeWatch>,
    /// The watch's `data_version` at the last successful upload, valid when
    /// `have_uploaded`.
    uploaded_version: i64,
    have_uploaded: bool,
}

/// Uploads the leader's database snapshot to S3 and restores it on standbys.
/// Exactly one instance is leader at a time (guaranteed by leader election), so
/// there is a single writer to the S3 snapshot object.
pub struct MetaSync {
    store: Arc<meta::Store>,
    api: Arc<dyn ObjectApi>,
    bucket: String,
    key: String,
    data_dir: PathBuf,
    interval: Duration,

    is_leader: AtomicBool,
    /// The current leadership term (Lease transition count). Snapshot uploads
    /// carry it; a stale leader with a lower fence is refused, preventing
    /// split-brain overwrites of a newer leader's metadata.
    fence: AtomicI64,

    /// Serializes sync cycles with promotion so `promote` never races a
    /// half-written snapshot download.
    sync_mu: tokio::sync::Mutex<SyncState>,

    uploads: IntCounterVec,
    downloads: IntCounterVec,
    promotions: IntCounterVec,
    last_sync_unix: Gauge,
    snapshot_bytes: Gauge,
}

/// The S3 user-metadata key holding the leadership term that wrote the
/// snapshot.
pub(crate) const FENCE_META_KEY: &str = "fence";

impl MetaSync {
    /// Builds a MetaSync and registers its metrics.
    pub fn new(o: MetaOptions) -> Arc<MetaSync> {
        let counter = |name: &str, help: &str| {
            IntCounterVec::new(Opts::new(name, help).namespace("forklift"), &["result"])
                .expect("valid objstore metric options")
        };
        let gauge = |name: &str, help: &str| {
            Gauge::with_opts(Opts::new(name, help).namespace("forklift"))
                .expect("valid objstore metric options")
        };
        let m = MetaSync {
            store: o.store,
            api: o.api,
            bucket: o.bucket,
            key: o.key,
            data_dir: o.data_dir,
            interval: o.interval,
            is_leader: AtomicBool::new(false),
            fence: AtomicI64::new(0),
            sync_mu: tokio::sync::Mutex::new(SyncState::default()),
            uploads: counter(
                "objstore_meta_uploads_total",
                "Metadata snapshot uploads to S3 by result.",
            ),
            downloads: counter(
                "objstore_meta_downloads_total",
                "Metadata snapshot downloads from S3 by result.",
            ),
            promotions: counter(
                "objstore_meta_promotions_total",
                "Leader promotions by how the authoritative database was obtained.",
            ),
            last_sync_unix: gauge(
                "objstore_meta_last_sync_timestamp_seconds",
                "Unix time of the last successful metadata sync cycle.",
            ),
            snapshot_bytes: gauge(
                "objstore_meta_snapshot_bytes",
                "Size of the last metadata snapshot transferred.",
            ),
        };
        if let Some(reg) = o.registry {
            for c in [&m.uploads, &m.downloads, &m.promotions] {
                reg.register(Box::new(c.clone()))
                    .expect("register objstore counter");
            }
            for g in [&m.last_sync_unix, &m.snapshot_bytes] {
                reg.register(Box::new(g.clone()))
                    .expect("register objstore gauge");
            }
        }
        Arc::new(m)
    }

    fn work_dir(&self) -> PathBuf {
        self.data_dir.join("objstore")
    }

    fn mark_synced(&self) {
        self.last_sync_unix
            .set(chrono::Utc::now().timestamp() as f64);
    }

    /// Downloads the latest snapshot from S3 and swaps it into the local
    /// database before the process serves traffic. It is required because the
    /// object-storage mode runs on an ephemeral volume that loses the database
    /// on restart. An empty bucket (no snapshot yet) is a no-op: the process
    /// starts with its local (fresh) database and the leader will upload it.
    pub async fn restore_on_boot(&self) -> Result<()> {
        let dst = self.work_dir().join("restore.db");
        let (found, _, _) = self.download(&dst).await?;
        if !found {
            tracing::info!(
                "objstore: no metadata snapshot in bucket; starting with local database"
            );
            return Ok(());
        }
        self.store
            .swap_from_snapshot(&dst)
            .await
            .map_err(|e| Error::Meta(e).context("apply boot snapshot"))?;
        tracing::info!("objstore: restored metadata from S3 snapshot");
        Ok(())
    }

    /// Executes the sync loop until `cancel` fires.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    if let Err(e) = self.sync().await
                        && !cancel.is_cancelled()
                    {
                        tracing::error!(err = %e, "objstore: meta sync failed");
                    }
                }
            }
        }
    }

    /// Runs one cycle: the leader uploads a fresh snapshot, a standby downloads
    /// the latest one and records it for promotion.
    pub(crate) async fn sync(&self) -> Result<()> {
        let mut st = self.sync_mu.lock().await;

        if self.is_leader.load(Ordering::SeqCst) {
            let uploaded = match self.upload(&mut st).await {
                Ok(v) => v,
                Err(e) => {
                    self.uploads.with_label_values(&["error"]).inc();
                    return Err(e.context("upload snapshot"));
                }
            };
            if uploaded {
                self.uploads.with_label_values(&["ok"]).inc();
            } else {
                self.uploads.with_label_values(&["unchanged"]).inc();
            }
            self.mark_synced();
            return Ok(());
        }

        // The object is re-downloaded only when it changed: a standby otherwise
        // pulls the full snapshot every interval whether or not the leader
        // uploaded a new one, which for a large database is most of its network
        // traffic.
        match self.api.head_object(&self.bucket, &self.key).await {
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => {
                // The optimisation is not worth a failed cycle: fall through to
                // the unconditional download, which reports its own error if S3
                // is down.
                tracing::warn!(err = %e, "objstore: head snapshot failed; downloading unconditionally");
            }
            Ok(head) => {
                if let Some(path) = st.snapshot_path.clone()
                    && st.snapshot_etag.is_some()
                    && st.snapshot_etag == head.e_tag
                    && tokio::fs::metadata(&path).await.is_ok()
                {
                    self.downloads.with_label_values(&["unchanged"]).inc();
                    self.mark_synced();
                    return Ok(());
                }
            }
        }
        let dst = self.work_dir().join("forklift.db");
        let (found, size, etag) = match self.download(&dst).await {
            Ok(v) => v,
            Err(e) => {
                self.downloads.with_label_values(&["error"]).inc();
                return Err(e.context("download snapshot"));
            }
        };
        if !found {
            return Ok(());
        }
        st.snapshot_path = Some(dst);
        st.snapshot_etag = etag;
        self.snapshot_bytes.set(size as f64);
        self.downloads.with_label_values(&["ok"]).inc();
        self.mark_synced();
        Ok(())
    }

    /// Writes a `VACUUM INTO` snapshot and puts it at the snapshot key. It
    /// returns false without touching S3 when nothing has been committed since
    /// the last successful upload. The database version is read before the
    /// snapshot is taken, so a commit that lands while the VACUUM runs (and is
    /// therefore not in this snapshot) is seen as a change on the next cycle.
    async fn upload(&self, st: &mut SyncState) -> Result<bool> {
        let (version, changed) = self.changed_since_upload(st).await;
        if !changed {
            return Ok(false);
        }
        let work_dir = self.work_dir();
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|e| Error::io("create objstore dir", e))?;
        let snap = work_dir.join("upload.db");
        let res = self.upload_snapshot(&snap, version, st).await;
        let _ = tokio::fs::remove_file(&snap).await;
        res
    }

    async fn upload_snapshot(&self, snap: &Path, version: i64, st: &mut SyncState) -> Result<bool> {
        self.store.snapshot(snap).await?;
        let size = tokio::fs::metadata(snap)
            .await
            .map_err(|e| Error::io("open snapshot", e))?
            .len() as i64;

        let mut put = PutObjectInput {
            bucket: self.bucket.clone(),
            key: self.key.clone(),
            body: PutBody::File(snap.to_path_buf()),
            content_length: size,
            metadata: HashMap::from([(
                FENCE_META_KEY.to_owned(),
                self.fence.load(Ordering::SeqCst).to_string(),
            )]),
            if_match: None,
            if_none_match: None,
        };
        // Fencing: never overwrite a snapshot written by a newer leadership
        // term. HEAD the current object; a higher stored fence means this
        // process is a stale ("zombie") leader and must not clobber the newer
        // state. The conditional write (If-Match on the observed ETag, or
        // If-None-Match for a first write) closes the HEAD->PUT race: a
        // concurrent writer makes one side lose with 412, which we treat as
        // "skip this cycle".
        match self.api.head_object(&self.bucket, &self.key).await {
            Ok(head) => {
                let stored = parse_fence(&head.metadata);
                let local = self.fence.load(Ordering::SeqCst);
                if stored > local {
                    return Err(Error::Fencing(format!(
                        "fencing: local term {local} older than stored {stored}; refusing to overwrite snapshot"
                    )));
                }
                put.if_match = head.e_tag;
            }
            Err(e) if e.is_not_found() => put.if_none_match = Some("*".to_owned()),
            Err(e) => return Err(e.context("head snapshot")),
        }
        if let Err(e) = self.api.put_object(put).await {
            if e.is_precondition_failed() {
                return Err(e.context(
                    "fencing: snapshot changed under us (concurrent writer); skipping cycle",
                ));
            }
            return Err(e);
        }
        self.snapshot_bytes.set(size as f64);
        st.uploaded_version = version;
        st.have_uploaded = true;
        Ok(true)
    }

    /// Reports the current database version and whether it differs from the one
    /// recorded at the last successful upload. Any doubt (no upload yet, no
    /// watch, a watch broken by a database swap) answers "changed": an extra
    /// snapshot is cheap next to a missed one. Callers hold `sync_mu`.
    async fn changed_since_upload(&self, st: &mut SyncState) -> (i64, bool) {
        if st.watch.is_none() {
            match self.store.new_change_watch().await {
                Ok(w) => st.watch = Some(w),
                Err(e) => {
                    tracing::warn!(err = %e, "objstore: change detection unavailable; uploading every cycle");
                    return (0, true);
                }
            }
        }
        let version = match st.watch.as_ref().expect("watch present").version().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(err = %e, "objstore: change watch failed; re-establishing");
                drop_watch(st);
                return (0, true);
            }
        };
        let changed = !st.have_uploaded || version != st.uploaded_version;
        (version, changed)
    }

    /// Fetches the snapshot to `dst` via a temp file and atomic rename. It
    /// returns `found = false` (no error) when the object does not exist yet,
    /// and the object's ETag so a caller can later tell whether the local copy
    /// is still the current one.
    async fn download(&self, dst: &Path) -> Result<(bool, i64, Option<String>)> {
        let out = match self.api.get_object(&self.bucket, &self.key).await {
            Ok(out) => out,
            Err(e) if e.is_not_found() => return Ok((false, 0, None)),
            Err(e) => return Err(e),
        };

        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::io("create objstore dir", e))?;
        }
        let tmp = {
            let mut p = dst.as_os_str().to_owned();
            p.push(".tmp");
            PathBuf::from(p)
        };
        let mut f = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| Error::io("create temp snapshot", e))?;
        let mut body = out.body;
        let copied = tokio::io::copy(&mut body, &mut f).await;
        let n = match copied {
            Ok(n) => n as i64,
            Err(e) => {
                drop(f);
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(Error::io("download snapshot", e));
            }
        };
        if let Err(e) = f.sync_all().await {
            drop(f);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::io("sync snapshot", e));
        }
        if let Err(e) = f.shutdown().await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::io("close snapshot", e));
        }
        drop(f);
        if let Err(e) = tokio::fs::rename(&tmp, dst).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Error::io("commit snapshot", e));
        }
        Ok((true, n, out.e_tag))
    }

    /// Called when this instance acquires leadership, before it reports Ready.
    /// The S3 snapshot is the single authority: this process always takes
    /// leadership on the object that is current *now*, never on its own local
    /// database. That rule is what keeps the database monotonic. Serving local
    /// data would let a re-elected former leader resurrect a state that S3 has
    /// already moved past, and a resurrected row whose blob the sweeper
    /// reclaimed in the meantime is a dangling reference no code path can
    /// repair.
    ///
    /// The copy fetched by the standby loop is reused only when its ETag still
    /// matches the live object, so the common case costs one HEAD rather than a
    /// full re-download. An empty bucket (no snapshot yet) is the one case where
    /// local data is authoritative: there is nothing to be behind.
    ///
    /// Leadership is only recorded once the authoritative database is in place.
    /// Doing it earlier would let a sync cycle that wins the race upload the
    /// not-yet-swapped local database under the new fence -- publishing exactly
    /// the stale state this function exists to discard. A failed promotion
    /// therefore leaves this process a standby, and its caller must not lead.
    pub async fn promote(&self, fence: i64) -> Result<()> {
        self.fence.store(fence, Ordering::SeqCst);
        let mut st = self.sync_mu.lock().await;
        // The swap below closes the read pool, taking any pinned watch
        // connection with it; the first upload of a term is unconditional
        // anyway.
        drop_watch(&mut st);
        let path = st.snapshot_path.take();
        let etag = st.snapshot_etag.take();

        let head = match self.api.head_object(&self.bucket, &self.key).await {
            Err(e) if e.is_not_found() => {
                self.is_leader.store(true, Ordering::SeqCst);
                self.promotions.with_label_values(&["empty_bucket"]).inc();
                tracing::info!("objstore: promoting with local data (no snapshot in bucket)");
                return Ok(());
            }
            Err(e) => {
                self.promotions.with_label_values(&["error"]).inc();
                return Err(e.context("head snapshot on promote"));
            }
            Ok(head) => head,
        };

        let mut result = "downloaded";
        let path = match path {
            Some(p) if etag.is_some() && etag == head.e_tag => {
                result = "cached";
                p
            }
            _ => {
                let dst = self.work_dir().join("promote.db");
                let (found, size, _) = match self.download(&dst).await {
                    Ok(v) => v,
                    Err(e) => {
                        self.promotions.with_label_values(&["error"]).inc();
                        return Err(e.context("download snapshot on promote"));
                    }
                };
                if !found {
                    // Raced a delete of the object; local data is all there is.
                    self.is_leader.store(true, Ordering::SeqCst);
                    self.promotions.with_label_values(&["empty_bucket"]).inc();
                    tracing::info!("objstore: promoting with local data (snapshot vanished)");
                    return Ok(());
                }
                self.snapshot_bytes.set(size as f64);
                dst
            }
        };
        if let Err(e) = self.store.swap_from_snapshot(&path).await {
            self.promotions.with_label_values(&["error"]).inc();
            return Err(Error::Meta(e).context("apply snapshot on promote"));
        }
        self.is_leader.store(true, Ordering::SeqCst);
        self.promotions.with_label_values(&[result]).inc();
        tracing::info!(source = result, "objstore: promoted with S3 snapshot");
        Ok(())
    }

    /// Called when leadership is lost. It flushes one final snapshot before
    /// standing down so the writes made since the last sync cycle reach S3
    /// instead of dying with this leader's local database, then the download
    /// loop resumes.
    pub async fn demote(&self) {
        self.final_sync().await;
        self.is_leader.store(false, Ordering::SeqCst);
        let mut st = self.sync_mu.lock().await;
        drop_watch(&mut st);
    }

    /// Uploads a last snapshot if this process is the leader. It is called on
    /// demotion and on shutdown, where the periodic loop would otherwise stop
    /// with up to one interval of writes only in the local database. It is
    /// best-effort: failures are logged, never returned, because nothing can be
    /// retried at that point.
    pub async fn final_sync(&self) {
        if !self.is_leader.load(Ordering::SeqCst) {
            return;
        }
        let mut st = self.sync_mu.lock().await;
        let uploaded = match self.upload(&mut st).await {
            Ok(v) => v,
            Err(e) => {
                self.uploads.with_label_values(&["error"]).inc();
                tracing::error!(err = %e, "objstore: final metadata snapshot upload failed");
                return;
            }
        };
        self.mark_synced();
        if !uploaded {
            self.uploads.with_label_values(&["unchanged"]).inc();
            tracing::info!(
                "objstore: final metadata snapshot skipped; nothing committed since last upload"
            );
            return;
        }
        self.uploads.with_label_values(&["ok"]).inc();
        tracing::info!("objstore: final metadata snapshot uploaded");
    }

    /// The standby's currently recorded snapshot copy, if any.
    #[cfg(test)]
    pub(crate) async fn snapshot_path(&self) -> Option<PathBuf> {
        self.sync_mu.lock().await.snapshot_path.clone()
    }

    /// Whether this process currently considers itself the leader.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }
}

/// Releases the pinned connection and forgets the upload baseline, so the next
/// upload is unconditional. Callers hold `sync_mu`.
fn drop_watch(st: &mut SyncState) {
    if let Some(w) = st.watch.take() {
        w.close();
    }
    st.have_uploaded = false;
}

/// Reads the fencing token from S3 object user-metadata. Keys are
/// case-insensitive; a missing or malformed value reads as 0.
pub(crate) fn parse_fence(md: &HashMap<String, String>) -> i64 {
    for (k, v) in md {
        if k.eq_ignore_ascii_case(FENCE_META_KEY)
            && let Ok(n) = v.parse::<i64>()
        {
            return n;
        }
    }
    0
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::meta;
    use crate::objstore::metasync::FENCE_META_KEY;
    use crate::objstore::*;

    /// A stored object with the user-metadata and ETag the fencing logic inspects.
    #[derive(Debug, Clone, Default)]
    struct MetaObj {
        data: Vec<u8>,
        meta: HashMap<String, String>,
        etag: String,
    }

    #[derive(Default)]
    struct FakeState {
        objects: HashMap<String, MetaObj>,
        put_n: usize,
        get_n: usize,
    }

    /// A minimal in-memory [`ObjectApi`] for MetaSync tests. It records
    /// user-metadata and assigns a unique ETag per write so fencing can be
    /// exercised.
    #[derive(Default)]
    struct FakeMetaS3 {
        state: parking_lot::Mutex<FakeState>,
    }

    impl FakeMetaS3 {
        fn new() -> Arc<FakeMetaS3> {
            Arc::new(FakeMetaS3::default())
        }

        fn gets(&self) -> usize {
            self.state.lock().get_n
        }

        fn puts(&self) -> usize {
            self.state.lock().put_n
        }

        fn object(&self, key: &str) -> Option<MetaObj> {
            self.state.lock().objects.get(key).cloned()
        }
    }

    #[async_trait]
    impl ObjectApi for FakeMetaS3 {
        async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput> {
            let b = match &input.body {
                PutBody::Bytes(b) => b.clone(),
                PutBody::File(p) => tokio::fs::read(p)
                    .await
                    .map_err(|e| Error::ObjectStore(e.to_string()))?,
            };
            let mut st = self.state.lock();
            st.put_n += 1;
            let etag = format!("etag-{}", st.put_n);
            st.objects.insert(
                input.key.clone(),
                MetaObj {
                    data: b,
                    meta: input.metadata.clone(),
                    etag: etag.clone(),
                },
            );
            Ok(PutObjectOutput { e_tag: Some(etag) })
        }

        async fn get_object(&self, _bucket: &str, key: &str) -> Result<GetObjectOutput> {
            let mut st = self.state.lock();
            let Some(o) = st.objects.get(key).cloned() else {
                return Err(Error::NotFound);
            };
            st.get_n += 1;
            Ok(GetObjectOutput {
                content_length: Some(o.data.len() as i64),
                e_tag: Some(o.etag),
                body: Box::new(std::io::Cursor::new(o.data)),
            })
        }

        async fn head_object(&self, _bucket: &str, key: &str) -> Result<HeadObjectOutput> {
            let st = self.state.lock();
            let Some(o) = st.objects.get(key) else {
                return Err(Error::NotFound);
            };
            Ok(HeadObjectOutput {
                e_tag: Some(o.etag.clone()),
                metadata: o.meta.clone(),
                content_length: Some(o.data.len() as i64),
            })
        }

        async fn delete_object(&self, _bucket: &str, key: &str) -> Result<()> {
            self.state.lock().objects.remove(key);
            Ok(())
        }
    }

    /// Fails `head_object` so promotion cannot establish which state is current.
    struct ErrHeadS3 {
        inner: Arc<FakeMetaS3>,
        err: String,
    }

    #[async_trait]
    impl ObjectApi for ErrHeadS3 {
        async fn put_object(&self, input: PutObjectInput) -> Result<PutObjectOutput> {
            self.inner.put_object(input).await
        }
        async fn get_object(&self, bucket: &str, key: &str) -> Result<GetObjectOutput> {
            self.inner.get_object(bucket, key).await
        }
        async fn head_object(&self, _bucket: &str, _key: &str) -> Result<HeadObjectOutput> {
            Err(Error::ObjectStore(self.err.clone()))
        }
        async fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
            self.inner.delete_object(bucket, key).await
        }
    }

    const KEY: &str = "forklift/meta/forklift.db";

    async fn open_store(dir: &Path) -> Arc<meta::Store> {
        Arc::new(
            meta::Store::open(dir.join("forklift.db"))
                .await
                .expect("open store"),
        )
    }

    fn new_meta_sync(
        store: Arc<meta::Store>,
        fake: Arc<dyn ObjectApi>,
        dir: &Path,
    ) -> Arc<MetaSync> {
        MetaSync::new(MetaOptions {
            store,
            api: fake,
            bucket: "test-bucket".into(),
            key: KEY.into(),
            data_dir: dir.to_path_buf(),
            interval: Duration::from_secs(60),
            registry: None,
        })
    }

    async fn set_user_version(store: &meta::Store, v: i64) {
        store
            .write(move |conn| {
                conn.pragma_update(None, "user_version", v)
                    .map_err(|e| meta::Error::sqlite("set user_version", e))
            })
            .await
            .expect("set marker");
    }

    async fn user_version(store: &meta::Store) -> i64 {
        store
            .read(|conn| {
                conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .map_err(|e| meta::Error::sqlite("read user_version", e))
            })
            .await
            .expect("read marker")
    }

    /// Uploads a leader snapshot and restores it onto a fresh pod, asserting the
    /// database content (via `PRAGMA user_version`) survives the round trip through
    /// S3.
    #[tokio::test]
    async fn meta_sync_round_trip() {
        let fake = FakeMetaS3::new();

        // Leader writes a marker and uploads a snapshot.
        let dir1 = tempfile::tempdir().expect("tempdir");
        let s1 = open_store(dir1.path()).await;
        set_user_version(&s1, 42).await;
        let m1 = new_meta_sync(Arc::clone(&s1), fake.clone(), dir1.path());
        m1.promote(1).await.expect("promote");
        m1.sync().await.expect("leader sync (upload)");
        s1.close();

        assert!(
            fake.object(KEY).is_some(),
            "snapshot not uploaded to expected key"
        );

        // A fresh pod restores from S3 on boot.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        m2.restore_on_boot().await.expect("restore on boot");

        let v = user_version(&s2).await;
        assert_eq!(
            v, 42,
            "user_version = {v}, want 42 (restore did not apply snapshot)"
        );
    }

    /// Verifies a fresh bucket is a clean no-op.
    #[tokio::test]
    async fn meta_sync_restore_empty_bucket() {
        let fake = FakeMetaS3::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open_store(dir.path()).await;
        let m = new_meta_sync(s, fake, dir.path());
        m.restore_on_boot()
            .await
            .expect("restore on empty bucket should be a no-op");
    }

    /// Verifies a standby records the latest snapshot and `promote` applies it.
    #[tokio::test]
    async fn meta_sync_standby_download() {
        let fake = FakeMetaS3::new();

        // Leader uploads.
        let dir1 = tempfile::tempdir().expect("tempdir");
        let s1 = open_store(dir1.path()).await;
        set_user_version(&s1, 7).await;
        let m1 = new_meta_sync(Arc::clone(&s1), fake.clone(), dir1.path());
        m1.promote(1).await.expect("promote leader");
        m1.sync().await.expect("leader upload");
        s1.close();

        // Standby downloads (not leader), then is promoted.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        m2.sync().await.expect("standby sync"); // standby branch: download
        assert!(
            m2.snapshot_path().await.is_some(),
            "standby did not record a downloaded snapshot"
        );
        m2.promote(1).await.expect("promote");
        let v = user_version(&s2).await;
        assert_eq!(v, 7, "user_version = {v}, want 7");
    }

    /// Verifies a leader from an older term (lower fencing token) cannot overwrite
    /// a snapshot written by a newer leader, preventing split-brain metadata loss.
    #[tokio::test]
    async fn meta_sync_fencing_refuses_stale_leader() {
        let fake = FakeMetaS3::new();

        // Current leader (term 5) uploads a snapshot.
        let dir1 = tempfile::tempdir().expect("tempdir");
        let s1 = open_store(dir1.path()).await;
        let m1 = new_meta_sync(Arc::clone(&s1), fake.clone(), dir1.path());
        m1.promote(5).await.expect("promote leader");
        m1.sync().await.expect("leader upload");
        let want = fake.object(KEY).expect("uploaded object").etag;
        let got = fake.object(KEY).expect("uploaded object").meta[FENCE_META_KEY].clone();
        assert_eq!(got, "5", "stored fence = {got:?}, want \"5\"");

        // A stale leader (term 3) must be refused and must not overwrite.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        m2.promote(3).await.expect("promote stale");
        m2.sync()
            .await
            .expect_err("stale leader (term 3) upload should be refused by fencing");
        assert_eq!(
            fake.object(KEY).expect("object").etag,
            want,
            "stale leader overwrote the snapshot despite fencing"
        );
    }

    /// The regression test for the bug class that produced dangling blob references
    /// in production: a re-elected former leader must not promote onto its own local
    /// database. Its local copy can hold state S3 has already moved past, and
    /// reviving it resurrects artifact rows whose blob bytes the sweeper reclaimed
    /// in the meantime.
    #[tokio::test]
    async fn meta_sync_promote_never_serves_stale_local() {
        let fake = FakeMetaS3::new();

        // Pod A leads on an empty bucket and publishes user_version 1.
        let dir_a = tempfile::tempdir().expect("tempdir");
        let sa = open_store(dir_a.path()).await;
        let ma = new_meta_sync(Arc::clone(&sa), fake.clone(), dir_a.path());
        set_user_version(&sa, 1).await;
        ma.promote(1).await.expect("promote A");
        ma.sync().await.expect("A upload");

        // A keeps writing, then loses leadership. Its local database is now ahead
        // of what it last published, and demote flushes that difference.
        set_user_version(&sa, 99).await;
        ma.demote().await;

        // Pod B takes over, adopts the published state, and advances it to 5.
        let dir_b = tempfile::tempdir().expect("tempdir");
        let sb = open_store(dir_b.path()).await;
        let mb = new_meta_sync(Arc::clone(&sb), fake.clone(), dir_b.path());
        mb.promote(2).await.expect("promote B");
        let adopted = user_version(&sb).await;
        assert_eq!(
            adopted, 99,
            "B promoted with user_version = {adopted}, want 99 (A's flushed state)"
        );
        set_user_version(&sb, 5).await;
        mb.sync().await.expect("B upload");

        // A is re-elected. Its local database still says 99; S3 says 5. S3 wins.
        ma.promote(3).await.expect("re-promote A");
        let got = user_version(&sa).await;
        assert_eq!(
            got, 5,
            "re-elected leader serves user_version = {got}, want 5; local state resurrected"
        );
    }

    /// Verifies the ETag shortcut: when the copy fetched by the standby loop is
    /// still current, promotion applies it without downloading the object again.
    #[tokio::test]
    async fn meta_sync_promote_reuses_matching_download() {
        let fake = FakeMetaS3::new();

        let dir1 = tempfile::tempdir().expect("tempdir");
        let s1 = open_store(dir1.path()).await;
        set_user_version(&s1, 11).await;
        let m1 = new_meta_sync(Arc::clone(&s1), fake.clone(), dir1.path());
        m1.promote(1).await.expect("promote");
        m1.sync().await.expect("leader upload");
        s1.close();

        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        m2.sync().await.expect("standby sync"); // standby branch: download
        let downloads = fake.gets();
        m2.promote(2).await.expect("promote");
        assert_eq!(
            fake.gets(),
            downloads,
            "promote re-downloaded a snapshot it already had (gets {downloads} -> {})",
            fake.gets()
        );
        let v = user_version(&s2).await;
        assert_eq!(v, 11, "user_version = {v}, want 11");
    }

    /// Verifies the writes made since the last periodic cycle reach S3 when
    /// leadership is handed over, instead of dying with the outgoing leader's
    /// ephemeral local volume.
    #[tokio::test]
    async fn meta_sync_demote_flushes_final_snapshot() {
        let fake = FakeMetaS3::new();

        let dir = tempfile::tempdir().expect("tempdir");
        let s = open_store(dir.path()).await;
        let m = new_meta_sync(Arc::clone(&s), fake.clone(), dir.path());
        m.promote(1).await.expect("promote");
        m.sync().await.expect("leader upload");
        let uploads = fake.puts();

        // A write lands after the last cycle, then leadership is lost.
        set_user_version(&s, 77).await;
        m.demote().await;
        assert_eq!(
            fake.puts(),
            uploads + 1,
            "uploads = {}, want {} (demotion did not flush a final snapshot)",
            fake.puts(),
            uploads + 1
        );

        // The flushed snapshot must carry the late write, and a demoted process
        // must not upload again.
        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        m2.restore_on_boot().await.expect("restore on boot");
        let v = user_version(&s2).await;
        assert_eq!(v, 77, "restored user_version = {v}, want 77");
        let after = fake.puts();
        m.final_sync().await;
        assert_eq!(fake.puts(), after, "FinalSync uploaded while not leader");
    }

    /// Verifies a promotion that cannot reach S3 leaves the process a standby.
    /// Recording leadership anyway would let the next sync cycle publish this pod's
    /// unverified local database over the real state.
    #[tokio::test]
    async fn meta_sync_promote_failure_does_not_lead() {
        let fake = FakeMetaS3::new();

        let dir = tempfile::tempdir().expect("tempdir");
        let s = open_store(dir.path()).await;

        // Seed a snapshot so the bucket is not empty, then break HEAD.
        let seed = new_meta_sync(Arc::clone(&s), fake.clone(), dir.path());
        seed.promote(1).await.expect("promote seed");
        seed.sync().await.expect("seed upload");
        let uploads = fake.puts();

        let broken: Arc<dyn ObjectApi> = Arc::new(ErrHeadS3 {
            inner: Arc::clone(&fake),
            err: "network down".into(),
        });
        let m = MetaSync::new(MetaOptions {
            store: Arc::clone(&s),
            api: broken,
            bucket: "test-bucket".into(),
            key: KEY.into(),
            data_dir: dir.path().to_path_buf(),
            interval: Duration::from_secs(60),
            registry: None,
        });
        m.promote(9)
            .await
            .expect_err("promote succeeded despite an unreachable snapshot");
        assert!(
            !m.is_leader(),
            "failed promotion left the process marked as leader"
        );
        // A sync cycle must now take the standby path, not upload.
        m.sync().await.expect("standby sync after failed promote");
        assert_eq!(
            fake.puts(),
            uploads,
            "uploads = {}, want {uploads} (stale local data was published)",
            fake.puts()
        );
    }

    /// Verifies an idle leader does not re-snapshot and re-upload an identical
    /// database every cycle, and that a commit (of any kind, a header write
    /// included) makes the next cycle upload.
    #[tokio::test]
    async fn meta_sync_leader_skips_unchanged_snapshot() {
        let fake = FakeMetaS3::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open_store(dir.path()).await;
        let m = new_meta_sync(Arc::clone(&s), fake.clone(), dir.path());
        m.promote(1).await.expect("promote");
        for i in 0..2 {
            m.sync().await.unwrap_or_else(|e| panic!("sync {i}: {e}"));
        }
        assert_eq!(
            fake.puts(),
            1,
            "uploads after two idle cycles = {}, want 1",
            fake.puts()
        );

        s.insert_audit_log(meta::AuditLog {
            repo_name: "r".into(),
            event: meta::EVENT_DOWNLOAD.into(),
            ..Default::default()
        })
        .await
        .expect("insert audit log");
        m.sync().await.expect("sync after commit");
        assert_eq!(
            fake.puts(),
            2,
            "uploads after a commit = {}, want 2",
            fake.puts()
        );
        m.sync().await.expect("idle sync");
        assert_eq!(
            fake.puts(),
            2,
            "uploads after another idle cycle = {}, want 2",
            fake.puts()
        );

        // A demoted-then-repromoted leader must not trust the stale baseline.
        m.demote().await;
        m.promote(2).await.expect("re-promote");
        m.sync().await.expect("sync after re-promotion");
        assert_eq!(
            fake.puts(),
            3,
            "uploads after re-promotion = {}, want 3 (first upload of a term is unconditional)",
            fake.puts()
        );
    }

    /// Verifies a standby re-downloads the snapshot only when the object changed,
    /// and still holds a promotable copy.
    #[tokio::test]
    async fn meta_sync_standby_skips_unchanged_download() {
        let fake = FakeMetaS3::new();

        let dir1 = tempfile::tempdir().expect("tempdir");
        let s1 = open_store(dir1.path()).await;
        set_user_version(&s1, 5).await;
        let m1 = new_meta_sync(Arc::clone(&s1), fake.clone(), dir1.path());
        m1.promote(1).await.expect("promote");
        m1.sync().await.expect("leader upload");

        let dir2 = tempfile::tempdir().expect("tempdir");
        let s2 = open_store(dir2.path()).await;
        let m2 = new_meta_sync(Arc::clone(&s2), fake.clone(), dir2.path());
        for i in 0..3 {
            m2.sync()
                .await
                .unwrap_or_else(|e| panic!("standby sync {i}: {e}"));
        }
        assert_eq!(
            fake.gets(),
            1,
            "downloads after three cycles against an unchanged object = {}, want 1",
            fake.gets()
        );

        // Leader publishes a new snapshot; the standby fetches it once.
        set_user_version(&s1, 6).await;
        m1.sync().await.expect("second leader upload");
        s1.close();
        m2.sync().await.expect("standby sync");
        m2.sync().await.expect("standby sync");
        assert_eq!(
            fake.gets(),
            2,
            "downloads after a new object = {}, want 2",
            fake.gets()
        );
        let downloads = fake.gets();
        m2.promote(2).await.expect("promote");
        assert_eq!(
            fake.gets(),
            downloads,
            "promote re-downloaded a snapshot the standby already held"
        );
        let v = user_version(&s2).await;
        assert_eq!(v, 6, "user_version = {v}, want 6");
    }

    /// The fencing token in S3 user-metadata: keys are case-insensitive, and a
    /// missing or malformed value reads as 0.
    #[test]
    fn parse_fence_cases() {
        use crate::objstore::metasync::parse_fence;
        assert_eq!(parse_fence(&HashMap::new()), 0);
        assert_eq!(
            parse_fence(&HashMap::from([("Fence".into(), "12".into())])),
            12
        );
        assert_eq!(
            parse_fence(&HashMap::from([("fence".into(), "not-a-number".into())])),
            0
        );
        assert_eq!(
            parse_fence(&HashMap::from([("other".into(), "7".into())])),
            0
        );
    }
}
