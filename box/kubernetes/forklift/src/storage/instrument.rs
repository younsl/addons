//! Latency metrics for any blob store backend.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use prometheus::{HistogramOpts, HistogramVec, Registry};
use tokio::io::AsyncRead;

use super::{BlobStore, Error, ReadSeekCloser, Result, SeekableStore, WalkableStore};

/// Wraps a [`WalkableStore`] and records every backend operation as a
/// `forklift_blobstore_operation_duration_seconds` observation. It also
/// forwards [`SeekableStore::open_seekable`], which the ZIP-validating upload
/// handlers look for via [`BlobStore::as_seekable`], so wrapping never hides
/// the inner store's seekable capability.
pub struct InstrumentedStore {
    inner: Arc<dyn WalkableStore>,
    backend: String,
    dur: HistogramVec,
}

/// Wraps `store` so put/open/open_seekable/exists/delete record their
/// duration and outcome. `backend` labels the series ("fs" or "s3") so one
/// dashboard covers both layouts. `walk_digests` passes through unmeasured:
/// its duration is dominated by the caller's per-digest callback, not backend
/// latency.
///
pub fn instrument(
    store: Arc<dyn WalkableStore>,
    backend: &str,
    registry: &Registry,
) -> InstrumentedStore {
    let opts = HistogramOpts::new(
        "blobstore_operation_duration_seconds",
        "Blob store backend operation latency by operation and outcome. Put includes streaming the source body.",
    )
    .namespace("forklift")
    // Puts of large artifacts stream the whole body and can run far past the
    // default 10s bucket cap, so the range extends to the 60s client timeout.
    .buckets(vec![0.005, 0.025, 0.1, 0.25, 1.0, 2.5, 10.0, 30.0, 60.0]);
    let dur = HistogramVec::new(opts, &["backend", "op", "result"])
        .expect("blobstore histogram definition is static and valid");
    registry
        .register(Box::new(dur.clone()))
        .expect("register forklift_blobstore_operation_duration_seconds");
    InstrumentedStore {
        inner: store,
        backend: backend.to_string(),
        dur,
    }
}

impl InstrumentedStore {
    /// Records one operation. A missing blob is an expected outcome, not a
    /// backend failure, so it gets its own result value instead of counting as
    /// an error.
    fn observe<T>(&self, op: &str, start: Instant, result: &Result<T>) {
        let outcome = match result {
            Ok(_) => "success",
            Err(Error::NotFound) => "not_found",
            Err(_) => "error",
        };
        self.dur
            .with_label_values(&[self.backend.as_str(), op, outcome])
            .observe(start.elapsed().as_secs_f64());
    }
}

#[async_trait]
impl BlobStore for InstrumentedStore {
    async fn put(&self, r: Pin<Box<dyn AsyncRead + Send>>) -> Result<(String, i64)> {
        let start = Instant::now();
        let res = self.inner.put(r).await;
        self.observe("put", start, &res);
        res
    }

    async fn open(&self, digest: &str) -> Result<(Box<dyn AsyncRead + Send + Unpin>, i64)> {
        let start = Instant::now();
        let res = self.inner.open(digest).await;
        self.observe("open", start, &res);
        res
    }

    async fn exists(&self, digest: &str) -> Result<bool> {
        let start = Instant::now();
        let res = self.inner.exists(digest).await;
        self.observe("exists", start, &res);
        res
    }

    async fn delete(&self, digest: &str) -> Result<()> {
        let start = Instant::now();
        let res = self.inner.delete(digest).await;
        self.observe("delete", start, &res);
        res
    }

    fn as_seekable(&self) -> Option<&dyn SeekableStore> {
        Some(self)
    }
}

#[async_trait]
impl SeekableStore for InstrumentedStore {
    /// Forwards to the inner store, failing when it has no seekable view.
    async fn open_seekable(&self, digest: &str) -> Result<(Box<dyn ReadSeekCloser>, i64)> {
        let Some(seekable) = self.inner.as_seekable() else {
            return Err(Error::Other(
                "blob store does not support seekable reads".into(),
            ));
        };
        let start = Instant::now();
        let res = seekable.open_seekable(digest).await;
        self.observe("open_seekable", start, &res);
        res
    }
}

#[async_trait]
impl WalkableStore for InstrumentedStore {
    async fn walk_digests(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<()> {
        self.inner.walk_digests(f).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use prometheus::Registry;

    use crate::storage::FsStore;
    use crate::storage::instrument::*;

    /// Returns the sample count for one (backend, op, result) series of
    /// `forklift_blobstore_operation_duration_seconds`, or 0 when absent.
    fn histogram_count(reg: &Registry, backend: &str, op: &str, result: &str) -> u64 {
        for mf in reg.gather() {
            if mf.name() != "forklift_blobstore_operation_duration_seconds" {
                continue;
            }
            for m in mf.get_metric() {
                let matches = m
                    .get_label()
                    .iter()
                    .any(|l| l.name() == "backend" && l.value() == backend)
                    && m.get_label()
                        .iter()
                        .any(|l| l.name() == "op" && l.value() == op)
                    && m.get_label()
                        .iter()
                        .any(|l| l.name() == "result" && l.value() == result);
                if matches {
                    return m.get_histogram().get_sample_count();
                }
            }
        }
        0
    }

    #[tokio::test]
    async fn instrumented_store() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(FsStore::new(dir.path()).unwrap());
        let reg = Registry::new();
        let s = instrument(fs, "fs", &reg);

        let (digest, _) = s
            .put(Box::pin(std::io::Cursor::new(b"hello".to_vec())))
            .await
            .unwrap();
        let (rc, _) = s.open(&digest).await.unwrap();
        drop(rc);
        let (src, _) = s.open_seekable(&digest).await.unwrap();
        drop(src);
        let err = s.open(&"0".repeat(64)).await.map(|(_, n)| n).unwrap_err();
        assert!(matches!(err, Error::NotFound), "expected NotFound: {err:?}");
        assert!(s.exists(&digest).await.unwrap(), "exists");
        s.delete(&digest).await.unwrap();

        for (op, result, want) in [
            ("put", "success", 1u64),
            ("open", "success", 1),
            ("open", "not_found", 1),
            ("open_seekable", "success", 1),
            ("exists", "success", 1),
            ("delete", "success", 1),
        ] {
            let got = histogram_count(&reg, "fs", op, result);
            assert_eq!(got, want, "{op}/{result}: sample count");
        }
    }

    #[test]
    fn instrumented_store_satisfies_store_traits() {
        fn assert_walkable<T: WalkableStore>() {}
        fn assert_seekable<T: SeekableStore>() {}
        assert_walkable::<InstrumentedStore>();
        assert_seekable::<InstrumentedStore>();
    }
}
