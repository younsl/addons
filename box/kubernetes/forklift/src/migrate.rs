//! Copies an object-storage deployment to another bucket or provider. Every
//! check runs before the first write (see [`preflight`]); blobs the metadata
//! names are copied and verified by digest, and the validated snapshot is
//! uploaded last.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use futures_util::{StreamExt, TryStreamExt, stream};

use crate::objstore::{self, ObjectApi, PutBody, PutObjectInput};
use crate::storage::{self, ClusterAdmin, WalkableStore};

pub mod cli;
pub mod lease;
pub mod postflight;
pub mod preflight;
mod snapshot;

pub use postflight::Verify;
pub use preflight::{Check, Status};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Refused(String),
    #[error("preflight failed:\n{}", preflight::render(.0))]
    Preflight(Vec<Check>),
    #[error("postflight failed:\n{}\n{remediation}", preflight::render(checks))]
    Postflight {
        checks: Vec<Check>,
        remediation: String,
    },
    #[error("blob {digest}: {source}")]
    Blob {
        digest: String,
        #[source]
        source: storage::Error,
    },
    #[error("blob {digest}: target stored digest {got}")]
    DigestMismatch { digest: String, got: String },
    #[error(transparent)]
    Storage(#[from] storage::Error),
    #[error(transparent)]
    Object(#[from] objstore::Error),
    #[error("{op}: {source}")]
    Io {
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// One side of a migration.
pub struct Endpoint {
    pub blobs: Arc<dyn WalkableStore>,
    pub objects: Arc<dyn ObjectApi>,
    pub bucket: String,
    pub prefix: String,
    pub meta_key: String,
    /// Normalised endpoint URL, compared to refuse overlapping locations.
    pub endpoint: String,
    pub admin: Option<Arc<dyn ClusterAdmin>>,
}

impl Endpoint {
    pub fn describe(&self) -> String {
        let endpoint = if self.endpoint.is_empty() {
            "s3://"
        } else {
            &self.endpoint
        };
        if self.prefix.is_empty() {
            format!("{endpoint} bucket={}", self.bucket)
        } else {
            format!("{endpoint} bucket={} prefix={}", self.bucket, self.prefix)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub concurrency: usize,
    pub dry_run: bool,
    pub overwrite_meta: bool,
    pub require_conditional_writes: bool,
    pub allow_missing_source_blobs: bool,
    pub verify: Verify,
    pub staging: PathBuf,
}

/// Lets the caller prove writers are stopped and tell when that stops being
/// true. Without Kubernetes the check is a warning the operator acknowledges.
pub struct Guard {
    pub checks: Vec<Check>,
    pub held: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Guard {
    pub fn unverified() -> Guard {
        Guard {
            checks: vec![Check::warn(
                "writers-stopped",
                "not verified outside Kubernetes; make sure every forklift replica is stopped",
            )],
            held: Box::new(|| true),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub required: i64,
    pub copied: i64,
    pub skipped: i64,
    pub bytes_copied: i64,
    pub meta_copied: bool,
    pub dry_run: bool,
    pub verified_blobs: i64,
    pub preflight: Vec<Check>,
    pub postflight: Vec<Check>,
}

pub async fn has_meta(e: &Endpoint) -> Result<bool> {
    match e.objects.head_object(&e.bucket, &e.meta_key).await {
        Ok(_) => Ok(true),
        Err(err) if err.is_not_found() => Ok(false),
        Err(err) => Err(err.into()),
    }
}

pub async fn migrate(
    src: &Endpoint,
    dst: &Endpoint,
    opts: &Options,
    guard: Guard,
) -> Result<Report> {
    let plan = preflight::run(src, dst, opts, guard.checks).await?;
    let mut report = Report {
        required: plan.snapshot.blobs.len() as i64,
        skipped: (plan.snapshot.blobs.len() - plan.missing.len()) as i64,
        dry_run: opts.dry_run,
        preflight: plan.checks.clone(),
        ..Report::default()
    };
    if opts.dry_run {
        report.copied = plan.missing.len() as i64;
        report.bytes_copied = plan.missing.values().map(|s| s.unwrap_or(0)).sum();
        return Ok(report);
    }

    let counters = Counters::default();
    let total = plan.missing.len();
    let held = &guard.held;
    stream::iter(plan.missing.keys().map(Ok))
        .try_for_each_concurrent(opts.concurrency.max(1), |digest| {
            let counters = &counters;
            async move {
                if !held() {
                    return Err(Error::Refused("lost the HA lease during the copy".into()));
                }
                let n = copy_blob(src, dst, digest).await?;
                counters.record(n, total);
                Ok::<_, Error>(())
            }
        })
        .await?;
    report.copied = counters.copied.load(Ordering::Relaxed);
    report.bytes_copied = counters.bytes.load(Ordering::Relaxed);

    verify_target(
        dst,
        &plan.snapshot.blobs,
        &plan.source_missing,
        opts.concurrency,
    )
    .await?;
    let current = src.objects.head_object(&src.bucket, &src.meta_key).await?;
    if current.e_tag != plan.snapshot.etag {
        return Err(Error::Refused(
            "source metadata changed during the copy; a writer is still running. Stop it and rerun (copied blobs are skipped)".into(),
        ));
    }
    if !held() {
        return Err(Error::Refused(
            "lost the HA lease before the metadata upload".into(),
        ));
    }
    upload_meta(dst, &plan.snapshot, opts.overwrite_meta).await?;
    report.meta_copied = true;

    let skipped: BTreeSet<String> = plan
        .snapshot
        .blobs
        .keys()
        .filter(|d| !plan.missing.contains_key(*d))
        .cloned()
        .collect();
    let outcome = postflight::run(
        src,
        dst,
        &plan.snapshot,
        &skipped,
        &plan.source_missing,
        opts,
    )
    .await;
    report.verified_blobs = outcome.verified_blobs as i64;
    report.postflight = outcome.checks.clone();
    if outcome.checks.iter().any(|c| c.status == Status::Fail) {
        let remediation = match postflight::remediate(dst, &outcome.bad_blobs).await {
            Ok(done) => done,
            Err(e) => format!("cleanup failed, remove the target metadata snapshot by hand: {e}"),
        };
        return Err(Error::Postflight {
            checks: outcome.checks,
            remediation,
        });
    }
    Ok(report)
}

async fn copy_blob(src: &Endpoint, dst: &Endpoint, digest: &str) -> Result<i64> {
    let blob_err = |source| Error::Blob {
        digest: digest.to_string(),
        source,
    };
    let (reader, _) = src.blobs.open(digest).await.map_err(blob_err)?;
    let (got, n) = dst.blobs.put(Box::pin(reader)).await.map_err(blob_err)?;
    if got != digest {
        return Err(Error::DigestMismatch {
            digest: digest.to_string(),
            got,
        });
    }
    Ok(n)
}

async fn verify_target(
    dst: &Endpoint,
    blobs: &BTreeMap<String, Option<i64>>,
    allowed_missing: &BTreeSet<String>,
    concurrency: usize,
) -> Result<()> {
    let missing: Vec<String> = stream::iter(blobs.keys().filter(|d| !allowed_missing.contains(*d)))
        .map(|d| async move { (d, dst.blobs.exists(d).await) })
        .buffer_unordered(concurrency.max(1))
        .filter_map(|(d, r)| async move {
            match r {
                Ok(true) => None,
                _ => Some(d.clone()),
            }
        })
        .collect()
        .await;
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::Refused(format!(
        "{} blobs the metadata names are not in the target after the copy (first: {}); the metadata was not copied",
        missing.len(),
        missing[0]
    )))
}

/// The fencing token is deliberately not copied: it is the source cluster's
/// Lease transition count, and a target leader whose Lease counts from zero
/// would otherwise refuse every snapshot upload as a stale term.
async fn upload_meta(
    dst: &Endpoint,
    snap: &snapshot::StagedSnapshot,
    overwrite: bool,
) -> Result<()> {
    dst.objects
        .put_object(PutObjectInput {
            bucket: dst.bucket.clone(),
            key: dst.meta_key.clone(),
            body: PutBody::File(snap.file.path().to_path_buf()),
            content_length: snap.size as i64,
            metadata: Default::default(),
            if_match: None,
            if_none_match: (!overwrite).then(|| "*".to_string()),
        })
        .await?;
    let head = dst.objects.head_object(&dst.bucket, &dst.meta_key).await?;
    if head.content_length != Some(snap.size as i64) {
        return Err(Error::Refused(format!(
            "target metadata is {:?} bytes after upload, want {}",
            head.content_length, snap.size
        )));
    }
    Ok(())
}

#[derive(Default)]
struct Counters {
    copied: AtomicI64,
    bytes: AtomicI64,
}

impl Counters {
    fn record(&self, bytes: i64, total: usize) {
        let done = self.copied.fetch_add(1, Ordering::Relaxed) + 1;
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        if done % 1000 == 0 || done as usize == total {
            tracing::info!(copied = done, total, "migrate: blobs copied");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;

    use async_trait::async_trait;
    use tokio::io::{AsyncRead, AsyncReadExt};

    use crate::migrate::snapshot::tests::database;
    use crate::migrate::*;
    use crate::objstore::{GetObjectOutput, HeadObjectOutput, PutObjectOutput};
    use crate::storage::{BlobStore, FsStore};

    type Item = (Vec<u8>, String, HashMap<String, String>);

    #[derive(Default)]
    pub(crate) struct Objects {
        pub(crate) items: parking_lot::Mutex<HashMap<String, Item>>,
        pub(crate) ignore_conditions: bool,
        pub(crate) bump_etag_after_heads: Option<usize>,
        /// Flips a byte of this key's body on upload, like a store that
        /// corrupts what it stores.
        pub(crate) corrupt_key: Option<String>,
        pub(crate) heads: parking_lot::Mutex<usize>,
    }

    #[async_trait]
    impl ObjectApi for Objects {
        async fn put_object(&self, input: PutObjectInput) -> objstore::Result<PutObjectOutput> {
            let mut data = match &input.body {
                PutBody::Bytes(b) => b.clone(),
                PutBody::File(p) => std::fs::read(p).unwrap(),
            };
            if self.corrupt_key.as_deref() == Some(input.key.as_str()) && !data.is_empty() {
                data[0] ^= 0xff;
            }
            let mut items = self.items.lock();
            if !self.ignore_conditions {
                if input.if_none_match.is_some() && items.contains_key(&input.key) {
                    return Err(objstore::Error::PreconditionFailed("exists".into()));
                }
                if let Some(want) = &input.if_match
                    && items.get(&input.key).map(|i| &i.1) != Some(want)
                {
                    return Err(objstore::Error::PreconditionFailed("etag".into()));
                }
            }
            let etag = format!("\"{}-{}\"", data.len(), items.len());
            items.insert(input.key, (data, etag.clone(), input.metadata));
            Ok(PutObjectOutput { e_tag: Some(etag) })
        }

        async fn get_object(&self, _bucket: &str, key: &str) -> objstore::Result<GetObjectOutput> {
            let (data, etag, _) = self
                .items
                .lock()
                .get(key)
                .cloned()
                .ok_or(objstore::Error::NotFound)?;
            Ok(GetObjectOutput {
                content_length: Some(data.len() as i64),
                e_tag: Some(etag),
                body: Box::new(std::io::Cursor::new(data)),
            })
        }

        async fn head_object(
            &self,
            _bucket: &str,
            key: &str,
        ) -> objstore::Result<HeadObjectOutput> {
            let (data, mut etag, metadata) = self
                .items
                .lock()
                .get(key)
                .cloned()
                .ok_or(objstore::Error::NotFound)?;
            let mut heads = self.heads.lock();
            *heads += 1;
            if self.bump_etag_after_heads.is_some_and(|n| *heads > n) {
                etag.push_str("-changed");
            }
            Ok(HeadObjectOutput {
                e_tag: Some(etag),
                metadata,
                content_length: Some(data.len() as i64),
            })
        }

        async fn delete_object(&self, _bucket: &str, key: &str) -> objstore::Result<()> {
            self.items.lock().remove(key);
            Ok(())
        }
    }

    pub(crate) struct Side {
        pub(crate) dir: tempfile::TempDir,
        pub(crate) blobs: Arc<FsStore>,
        pub(crate) objects: Arc<Objects>,
        pub(crate) endpoint: String,
    }

    pub(crate) fn side(endpoint: &str) -> Side {
        let dir = tempfile::tempdir().unwrap();
        Side {
            blobs: Arc::new(FsStore::new(dir.path()).unwrap()),
            dir,
            objects: Arc::new(Objects::default()),
            endpoint: endpoint.into(),
        }
    }

    pub(crate) fn endpoint(s: &Side) -> Endpoint {
        Endpoint {
            blobs: s.blobs.clone(),
            objects: s.objects.clone(),
            bucket: "b".into(),
            prefix: String::new(),
            meta_key: "meta/forklift.db".into(),
            endpoint: s.endpoint.clone(),
            admin: None,
        }
    }

    fn reader(data: &[u8]) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(std::io::Cursor::new(data.to_vec()))
    }

    pub(crate) fn opts(staging: &std::path::Path) -> Options {
        Options {
            concurrency: 4,
            dry_run: false,
            overwrite_meta: false,
            require_conditional_writes: false,
            allow_missing_source_blobs: false,
            verify: Verify::Full,
            staging: staging.to_path_buf(),
        }
    }

    pub(crate) fn ok_guard() -> Guard {
        Guard {
            checks: vec![Check::pass("writers-stopped", "test")],
            held: Box::new(|| true),
        }
    }

    /// Five blobs on the source, all named by its metadata snapshot, plus one
    /// unreferenced blob that must not be copied.
    pub(crate) async fn seeded() -> (Side, Vec<String>) {
        let src = side("http://src");
        let mut named = Vec::new();
        for i in 0..5 {
            let (d, n) = src
                .blobs
                .put(reader(format!("blob-{i}").as_bytes()))
                .await
                .unwrap();
            named.push((d, n));
        }
        src.blobs.put(reader(b"orphan")).await.unwrap();
        let db = src.dir.path().join("snapshot.db");
        database(&db, &named, &[]);
        src.objects
            .put_object(PutObjectInput {
                bucket: "b".into(),
                key: "meta/forklift.db".into(),
                body: PutBody::File(db),
                content_length: 0,
                metadata: HashMap::from([("fence".into(), "7".into())]),
                if_match: None,
                if_none_match: None,
            })
            .await
            .unwrap();
        (src, named.into_iter().map(|(d, _)| d).collect())
    }

    pub(crate) async fn count(store: &FsStore) -> usize {
        let mut n = 0;
        store
            .walk_digests(&mut |_| {
                n += 1;
                Ok(())
            })
            .await
            .unwrap();
        n
    }

    #[tokio::test]
    async fn copies_named_blobs_then_meta_and_reruns_incrementally() {
        let (src, digests) = seeded().await;
        let dst = side("http://dst");
        dst.blobs.put(reader(b"blob-0")).await.unwrap();
        let staging = tempfile::tempdir().unwrap();

        let report = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap();
        assert_eq!((report.required, report.copied, report.skipped), (5, 4, 1));
        assert_eq!(report.bytes_copied, 4 * 6);
        assert!(report.meta_copied);
        assert!(report.preflight.iter().all(|c| c.status != Status::Fail));
        assert_eq!(count(&dst.blobs).await, 5, "orphan not copied");
        for d in &digests {
            let (mut r, _) = dst.blobs.open(d).await.unwrap();
            let mut got = Vec::new();
            r.read_to_end(&mut got).await.unwrap();
            assert!(got.starts_with(b"blob-"));
        }
        let head = dst
            .objects
            .head_object("b", "meta/forklift.db")
            .await
            .unwrap();
        assert!(head.metadata.is_empty(), "fence must not be copied");
        assert_eq!(
            dst.objects.items.lock()["meta/forklift.db"].0,
            src.objects.items.lock()["meta/forklift.db"].0,
            "snapshot bytes copied verbatim"
        );
        assert!(
            dst.objects
                .items
                .lock()
                .keys()
                .all(|k| !k.contains(".forklift-migrate")),
            "probe objects cleaned up"
        );

        let again = Options {
            overwrite_meta: true,
            ..opts(staging.path())
        };
        let report = migrate(&endpoint(&src), &endpoint(&dst), &again, ok_guard())
            .await
            .unwrap();
        assert_eq!((report.copied, report.skipped), (0, 5));
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let report = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &Options {
                dry_run: true,
                ..opts(staging.path())
            },
            ok_guard(),
        )
        .await
        .unwrap();
        assert_eq!((report.copied, report.bytes_copied), (5, 30));
        assert!(!report.meta_copied && report.dry_run);
        assert_eq!(count(&dst.blobs).await, 0);
        assert!(
            dst.objects.items.lock().is_empty(),
            "dry run leaves no probe objects either"
        );
    }

    #[tokio::test]
    async fn refuses_meta_written_during_copy() {
        let (src, _) = seeded().await;
        let src = Side {
            objects: Arc::new(Objects {
                items: parking_lot::Mutex::new(src.objects.items.lock().clone()),
                bump_etag_after_heads: Some(0),
                ..Objects::default()
            }),
            ..src
        };
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let err = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("changed during the copy"), "{err}");
        assert!(!dst.objects.items.lock().contains_key("meta/forklift.db"));
    }

    #[tokio::test]
    async fn refuses_when_the_lease_is_lost() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let guard = Guard {
            checks: vec![Check::pass("writers-stopped", "test")],
            held: Box::new(|| false),
        };
        let err = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            guard,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("lost the HA lease"), "{err}");
        assert!(!dst.objects.items.lock().contains_key("meta/forklift.db"));
    }

    fn blob_path(side: &Side, digest: &str) -> std::path::PathBuf {
        side.dir
            .path()
            .join("blobs")
            .join(&digest[0..2])
            .join(&digest[2..4])
            .join(digest)
    }

    fn postflight_ids(checks: &[Check], status: Status) -> Vec<&'static str> {
        checks
            .iter()
            .filter(|c| c.status == status)
            .map(|c| c.id)
            .collect()
    }

    #[tokio::test]
    async fn postflight_passes_and_rehashes_every_blob_in_full_mode() {
        let (src, _) = seeded().await;
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let report = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap();
        let ids: Vec<&str> = report.postflight.iter().map(|c| c.id).collect();
        assert_eq!(ids, ["PV01", "PV02", "PV03", "PV04"]);
        assert!(
            report.postflight.iter().all(|c| c.status == Status::Pass),
            "{:?}",
            report.postflight
        );
        assert_eq!(report.verified_blobs, 5);
        assert!(
            preflight::render(&report.postflight)
                .ends_with("postflight: 4 checks, 4 passed, 0 warned, 0 failed, 0 skipped")
        );
    }

    #[tokio::test]
    async fn a_corrupt_preexisting_blob_is_caught_removed_and_recopied() {
        let (src, digests) = seeded().await;
        let dst = side("http://dst");
        let victim = &digests[2];
        dst.blobs.put(reader(b"blob-2")).await.unwrap();
        std::fs::write(blob_path(&dst, victim), b"BLOB-2").unwrap();
        let staging = tempfile::tempdir().unwrap();

        let err = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap_err();
        let Error::Postflight {
            checks,
            remediation,
        } = &err
        else {
            panic!("want a postflight failure, got {err}");
        };
        assert_eq!(postflight_ids(checks, Status::Fail), ["PV03"], "{err}");
        assert!(
            remediation.contains("removed 1 bad target blobs"),
            "{remediation}"
        );
        assert!(
            !dst.blobs.exists(victim).await.unwrap(),
            "corrupt blob deleted"
        );
        assert!(
            !dst.objects.items.lock().contains_key("meta/forklift.db"),
            "metadata withdrawn"
        );

        let report = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap();
        assert_eq!(report.copied, 1, "only the removed blob is copied again");
        assert!(report.postflight.iter().all(|c| c.status == Status::Pass));
    }

    #[tokio::test]
    async fn a_wrong_size_is_caught_even_without_rehashing() {
        let (src, digests) = seeded().await;
        let dst = side("http://dst");
        dst.blobs.put(reader(b"blob-1")).await.unwrap();
        std::fs::write(blob_path(&dst, &digests[1]), b"blob-1 and more").unwrap();
        let staging = tempfile::tempdir().unwrap();
        let o = Options {
            verify: Verify::Off,
            ..opts(staging.path())
        };
        let err = migrate(&endpoint(&src), &endpoint(&dst), &o, ok_guard())
            .await
            .unwrap_err();
        let Error::Postflight { checks, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(postflight_ids(checks, Status::Fail), ["PV02"]);
        assert_eq!(postflight_ids(checks, Status::Skip), ["PV03"], "verify off");
        assert!(!dst.blobs.exists(&digests[1]).await.unwrap());
    }

    #[tokio::test]
    async fn a_mangled_metadata_upload_is_caught() {
        let (src, _) = seeded().await;
        let dst = Side {
            objects: Arc::new(Objects {
                corrupt_key: Some("meta/forklift.db".into()),
                ..Objects::default()
            }),
            ..side("http://dst")
        };
        let staging = tempfile::tempdir().unwrap();
        let err = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap_err();
        let Error::Postflight { checks, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(postflight_ids(checks, Status::Fail), ["PV01"], "{err}");
        assert!(!dst.objects.items.lock().contains_key("meta/forklift.db"));
    }

    #[tokio::test]
    async fn a_source_write_after_the_upload_is_caught() {
        let (src, _) = seeded().await;
        let src = Side {
            objects: Arc::new(Objects {
                items: parking_lot::Mutex::new(src.objects.items.lock().clone()),
                bump_etag_after_heads: Some(1),
                ..Objects::default()
            }),
            ..src
        };
        let dst = side("http://dst");
        let staging = tempfile::tempdir().unwrap();
        let err = migrate(
            &endpoint(&src),
            &endpoint(&dst),
            &opts(staging.path()),
            ok_guard(),
        )
        .await
        .unwrap_err();
        let Error::Postflight { checks, .. } = &err else {
            panic!("{err}");
        };
        assert_eq!(postflight_ids(checks, Status::Fail), ["PV04"]);
    }
}
