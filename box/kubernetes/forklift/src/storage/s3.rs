//! S3-backed blob store. Objects mirror the filesystem layout so both backends
//! enumerate digests in the same order and the rest of the system is
//! backend-agnostic.

use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_smithy_http_client::tls;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use super::{
    BlobStore, Error, ReadSeekCloser, Result, SeekableStore, StagedFile, WalkableStore, blocking,
    stream_and_hash, valid_digest,
};

/// Configures the S3 client. Region/endpoint/credentials are optional: an
/// empty region or credentials falls back to the AWS default chain, which
/// covers EKS IRSA and EKS Pod Identity automatically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    /// Custom endpoint for MinIO/localstack; empty uses AWS.
    pub endpoint: String,
    /// True for MinIO-style path addressing.
    pub force_path_style: bool,
    /// Optional; empty uses the default credential chain.
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// Builds an S3 [`Client`] from `cfg`. The SDK's default credential chain
/// resolves IRSA (web identity) and EKS Pod Identity (container credentials)
/// with no extra code; static keys are used only when both are set. The HTTP
/// client is rustls over ring so the binary carries no OpenSSL.
pub async fn new_s3_client(cfg: &S3Config) -> Result<Client> {
    let http_client = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https();
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).http_client(http_client);
    if !cfg.region.is_empty() {
        loader = loader.region(Region::new(cfg.region.clone()));
    }
    if !cfg.access_key_id.is_empty() && !cfg.secret_access_key.is_empty() {
        loader = loader.credentials_provider(Credentials::from_keys(
            &cfg.access_key_id,
            &cfg.secret_access_key,
            None,
        ));
    }
    let sdk = loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&sdk);
    if !cfg.endpoint.is_empty() {
        builder = builder.endpoint_url(&cfg.endpoint);
    }
    builder = builder.force_path_style(cfg.force_path_style);
    Ok(Client::from_conf(builder.build()))
}

/// An S3-backed [`BlobStore`]. Blobs are immutable and content-addressed, so
/// every replica can read and write the same bucket concurrently without
/// coordination: this is what lets an HA deployment share blobs without a
/// ReadWriteMany volume. Objects are laid out as
/// `<prefix>/blobs/aa/bb/<digest>`, mirroring [`super::FsStore`] so
/// [`WalkableStore::walk_digests`] yields the same lexicographic (== digest)
/// order.
///
/// S3 has been strongly read-after-write and list consistent since Dec 2020,
/// so no read-retry workarounds are needed.
#[derive(Debug, Clone)]
pub struct S3BlobStore {
    client: Client,
    bucket: String,
    /// Normalized: no leading/trailing slash, may be "".
    prefix: String,
    /// Local dir for hashing during put.
    temp_dir: PathBuf,
}

impl S3BlobStore {
    /// Builds an S3-backed blob store. `temp_dir` holds the transient file each
    /// put streams through while hashing (removed immediately after upload); in
    /// s3 mode this lives on the pod's emptyDir.
    pub async fn new(cfg: &S3Config, temp_dir: impl AsRef<Path>) -> Result<Self> {
        if cfg.bucket.is_empty() {
            return Err(Error::Other("s3 blob store requires a bucket".into()));
        }
        let client = new_s3_client(cfg).await?;
        let temp_dir = temp_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&temp_dir).map_err(|e| Error::io("create blob temp dir", e))?;
        Ok(S3BlobStore {
            client,
            bucket: cfg.bucket.clone(),
            prefix: cfg.prefix.trim_matches('/').to_string(),
            temp_dir,
        })
    }

    /// Returns the underlying S3 client so co-located components (e.g. the
    /// metadata sync) can share one configured client and bucket.
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Fan out by the first two byte-pairs, matching `FsStore`'s on-disk layout.
    fn key(&self, digest: &str) -> String {
        join_key(&[&self.prefix, "blobs", &digest[0..2], &digest[2..4], digest])
    }

    fn blobs_prefix(&self) -> String {
        join_key(&[&self.prefix, "blobs"]) + "/"
    }

    async fn exists_key(&self, key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(Error::s3("head blob", e)),
        }
    }
}

fn join_key(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

#[async_trait]
impl BlobStore for S3BlobStore {
    /// Streams `r` into a local temp file while hashing, then uploads to the
    /// digest key. Because the digest (and therefore the key) is known only
    /// after the full stream is read, the temp file lets the upload target the
    /// final key directly: no temp S3 key or server-side copy. Re-putting an
    /// existing blob skips the upload.
    async fn put(&self, r: Pin<Box<dyn AsyncRead + Send>>) -> Result<(String, i64)> {
        let temp_dir = self.temp_dir.clone();
        let tmp = blocking(move || {
            tempfile::Builder::new()
                .prefix("blob-")
                .tempfile_in(temp_dir)
        })
        .await?
        .map_err(|e| Error::io("create temp blob", e))?;
        let (std_file, tmp_path) = tmp.into_parts();
        let mut file = tokio::fs::File::from_std(std_file);

        let (digest, n) = stream_and_hash(r, &mut file, "buffer blob").await?;
        drop(file);
        let key = self.key(&digest);

        // Identical bytes by construction; skip the upload if already present.
        if self.exists_key(&key).await? {
            return Ok((digest, n));
        }

        // Reading the body from the file path keeps it out of memory and lets
        // the SDK set Content-Length and replay the body on retry. A single
        // PutObject caps at 5 GiB, far above any real package artifact.
        let body = ByteStream::read_from()
            .path(&*tmp_path)
            .length(Length::Exact(n as u64))
            .build()
            .await
            .map_err(|e| Error::s3("rewind temp blob", e))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body)
            .content_length(n)
            .send()
            .await
            .map_err(|e| Error::s3("upload blob", e))?;
        Ok((digest, n))
    }

    async fn open(&self, digest: &str) -> Result<(Box<dyn AsyncRead + Send + Unpin>, i64)> {
        if !valid_digest(digest) {
            return Err(Error::NotFound);
        }
        let out = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(digest))
            .send()
            .await
        {
            Ok(out) => out,
            Err(e) if is_not_found(&e) => return Err(Error::NotFound),
            Err(e) => return Err(Error::s3("open blob", e)),
        };
        let size = out.content_length.unwrap_or(0);
        Ok((Box::new(out.body.into_async_read()), size))
    }

    async fn exists(&self, digest: &str) -> Result<bool> {
        if !valid_digest(digest) {
            return Ok(false);
        }
        self.exists_key(&self.key(digest)).await
    }

    /// Deleting a missing blob is a no-op.
    async fn delete(&self, digest: &str) -> Result<()> {
        if !valid_digest(digest) {
            return Ok(());
        }
        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(digest))
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(Error::s3("delete blob", e)),
        }
    }

    fn as_seekable(&self) -> Option<&dyn SeekableStore> {
        Some(self)
    }
}

#[async_trait]
impl SeekableStore for S3BlobStore {
    /// Downloads an immutable object into the store's explicitly configured
    /// private staging directory. ZIP validators use this instead of buffering
    /// package bytes in memory or relying on the container's /tmp.
    async fn open_seekable(&self, digest: &str) -> Result<(Box<dyn ReadSeekCloser>, i64)> {
        let (mut reader, size) = self.open(digest).await?;
        let temp_dir = self.temp_dir.clone();
        let staged = blocking(move || {
            tempfile::Builder::new()
                .prefix("seekable-blob-")
                .tempfile_in(temp_dir)
        })
        .await?
        .map_err(|e| Error::io("create seekable blob", e))?;
        let (std_file, tmp_path) = staged.into_parts();
        let mut file = tokio::fs::File::from_std(std_file);

        // Read one byte past the advertised size so a body longer than its
        // Content-Length is detected instead of silently truncated.
        let mut limited = (&mut reader).take(size as u64 + 1);
        let mut buf = vec![0u8; 64 * 1024];
        let mut written: i64 = 0;
        loop {
            let read = match limited.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    return Err(Error::Other(format!(
                        "stage seekable blob: expected {size} bytes, wrote {written}: {e}"
                    )));
                }
            };
            if let Err(e) = file.write_all(&buf[..read]).await {
                return Err(Error::Other(format!(
                    "stage seekable blob: expected {size} bytes, wrote {written}: {e}"
                )));
            }
            written += read as i64;
        }
        if written != size {
            return Err(Error::Other(format!(
                "stage seekable blob: expected {size} bytes, wrote {written}"
            )));
        }
        file.flush()
            .await
            .map_err(|e| Error::io("stage seekable blob", e))?;
        let mut std_file = file.into_std().await;
        std_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| Error::io("rewind seekable blob", e))?;
        // Hand removal over to the StagedFile, which drops it with the reader.
        let path = tmp_path
            .keep()
            .map_err(|e| Error::io("keep seekable blob", e.error))?;
        Ok((Box::new(StagedFile::removing(std_file, path)), size))
    }
}

#[async_trait]
impl WalkableStore for S3BlobStore {
    /// S3 returns keys in lexicographic order, which equals digest order given
    /// the layout, so replication's streaming merge works unchanged. Walking
    /// stops at the first error `f` returns.
    async fn walk_digests(
        &self,
        f: &mut (dyn for<'a> FnMut(&'a str) -> Result<()> + Send),
    ) -> Result<()> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(self.blobs_prefix())
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| Error::s3("list blobs", e))?;
            for obj in page.contents() {
                let key = obj.key().unwrap_or_default();
                let d = key.trim_end_matches('/').rsplit('/').next().unwrap_or(key);
                if !valid_digest(d) {
                    continue;
                }
                f(d)?;
            }
        }
        Ok(())
    }
}

/// Reports whether `err` is an S3 "no such key"/404, across the SDK's typed
/// errors and the generic HTTP response. Exported so co-located components
/// (e.g. objstore) can treat a missing object as absence rather than failure.
pub fn is_not_found<E>(err: &SdkError<E, HttpResponse>) -> bool
where
    E: ProvideErrorMetadata,
{
    if matches!(err.code(), Some("NoSuchKey" | "NotFound")) {
        return true;
    }
    matches!(err.raw_response(), Some(r) if r.status().as_u16() == 404)
}

/// Reports whether `err` is an S3 412 Precondition Failed, returned when a
/// conditional write (If-Match / If-None-Match) is rejected because another
/// writer changed the object first. Co-located components (e.g. objstore
/// fencing) treat it as "lost the race, skip this cycle".
pub fn is_precondition_failed<E>(err: &SdkError<E, HttpResponse>) -> bool {
    matches!(err.raw_response(), Some(r) if r.status().as_u16() == 412)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use sha2::{Digest as _, Sha256};
    use tokio::io::AsyncRead;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use crate::storage::WalkableStore;
    use crate::storage::s3::*;

    /// An in-memory path-style S3 endpoint. `page_size`, when set, caps
    /// ListObjectsV2 pages to exercise pagination.
    #[derive(Clone)]
    struct FakeS3 {
        objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
        page_size: Option<usize>,
    }

    impl FakeS3 {
        fn new() -> Self {
            FakeS3 {
                objects: Arc::new(Mutex::new(BTreeMap::new())),
                page_size: None,
            }
        }

        fn with_page_size(page_size: usize) -> Self {
            FakeS3 {
                objects: Arc::new(Mutex::new(BTreeMap::new())),
                page_size: Some(page_size),
            }
        }

        fn len(&self) -> usize {
            self.objects.lock().len()
        }

        /// Strips the bucket segment from a path-style request path.
        fn key_of(request: &Request) -> String {
            request
                .url
                .path()
                .trim_start_matches('/')
                .split_once('/')
                .map(|(_bucket, key)| key.to_string())
                .unwrap_or_default()
        }

        fn list(&self, request: &Request) -> ResponseTemplate {
            let params: BTreeMap<String, String> = request
                .url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let prefix = params.get("prefix").cloned().unwrap_or_default();
            let objects = self.objects.lock();
            let keys: Vec<String> = objects
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            drop(objects);

            let start = match params.get("continuation-token") {
                Some(tok) => keys.partition_point(|k| k.as_str() < tok.as_str()),
                None => 0,
            };
            let mut limit = keys.len() - start;
            if let Some(page) = self.page_size
                && page < limit
            {
                limit = page;
            }
            if let Some(max) = params.get("max-keys").and_then(|v| v.parse::<usize>().ok())
                && max > 0
                && max < limit
            {
                limit = max;
            }
            let end = start + limit;

            let mut body = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>test-bucket</Name>"#,
            );
            body.push_str(&format!("<KeyCount>{limit}</KeyCount>"));
            if end < keys.len() {
                body.push_str("<IsTruncated>true</IsTruncated>");
                body.push_str(&format!(
                    "<NextContinuationToken>{}</NextContinuationToken>",
                    keys[end]
                ));
            } else {
                body.push_str("<IsTruncated>false</IsTruncated>");
            }
            for k in &keys[start..end] {
                body.push_str(&format!(
                    "<Contents><Key>{k}</Key><Size>0</Size></Contents>"
                ));
            }
            body.push_str("</ListBucketResult>");
            ResponseTemplate::new(200).set_body_raw(body, "application/xml")
        }
    }

    impl Respond for FakeS3 {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let key = FakeS3::key_of(request);
            match request.method.as_str() {
                "GET" if request.url.query_pairs().any(|(k, _)| k == "list-type") => {
                    self.list(request)
                }
                "GET" => match self.objects.lock().get(&key) {
                    Some(b) => ResponseTemplate::new(200).set_body_bytes(b.clone()),
                    None => no_such_key(),
                },
                "HEAD" => match self.objects.lock().get(&key) {
                    Some(b) => ResponseTemplate::new(200)
                        .append_header("content-length", b.len().to_string().as_str()),
                    // A HEAD response carries no body, so the SDK maps the bare 404
                    // to `NotFound` the way real S3 does.
                    None => ResponseTemplate::new(404),
                },
                "PUT" => {
                    let body = decode_body(request);
                    self.objects.lock().insert(key, body);
                    ResponseTemplate::new(200).append_header("etag", "\"fake\"")
                }
                "DELETE" => {
                    self.objects.lock().remove(&key);
                    ResponseTemplate::new(204)
                }
                other => panic!("unexpected method {other}"),
            }
        }
    }

    fn no_such_key() -> ResponseTemplate {
        ResponseTemplate::new(404).set_body_raw(
        r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#,
        "application/xml",
    )
    }

    /// Undoes the SDK's `aws-chunked` framing when it streams a body with trailing
    /// checksums, so the fake stores the original bytes.
    fn decode_body(request: &Request) -> Vec<u8> {
        let chunked = request
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("aws-chunked"));
        if !chunked {
            return request.body.clone();
        }
        let mut out = Vec::new();
        let mut rest = request.body.as_slice();
        while let Some(nl) = rest.windows(2).position(|w| w == b"\r\n") {
            let header = &rest[..nl];
            // A chunk header is "<hex size>[;chunk-signature=...]".
            let size_hex = header.split(|b| *b == b';').next().unwrap_or_default();
            let size = usize::from_str_radix(&String::from_utf8_lossy(size_hex), 16).unwrap_or(0);
            rest = &rest[nl + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
        out
    }

    async fn new_test_s3_store(fake: FakeS3) -> (S3BlobStore, MockServer, tempfile::TempDir) {
        let server = MockServer::start().await;
        Mock::given(any()).respond_with(fake).mount(&server).await;
        let tmp = tempfile::tempdir().unwrap();
        let store = S3BlobStore::new(
            &S3Config {
                bucket: "test-bucket".into(),
                prefix: "forklift".into(),
                region: "us-east-1".into(),
                endpoint: server.uri(),
                force_path_style: true,
                access_key_id: "test".into(),
                secret_access_key: "secret".into(),
            },
            tmp.path(),
        )
        .await
        .unwrap();
        (store, server, tmp)
    }

    fn reader(data: Vec<u8>) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(std::io::Cursor::new(data))
    }

    #[tokio::test]
    async fn s3_blob_store_round_trip() {
        let fake = FakeS3::new();
        let (s, _server, _tmp) = new_test_s3_store(fake).await;

        let data = b"hello forklift over s3".to_vec();
        let want_hex = hex::encode(Sha256::digest(&data));

        let (digest, n) = s.put(reader(data.clone())).await.expect("put");
        assert_eq!(digest, want_hex, "digest");
        assert_eq!(n, data.len() as i64, "size");

        assert!(s.exists(&digest).await.unwrap(), "exists");

        let (mut rc, size) = s.open(&digest).await.expect("open");
        assert_eq!(size, data.len() as i64, "open size");
        let mut got = Vec::new();
        rc.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, data, "read");
        drop(rc);

        // The temp file used for hashing must not linger.
        let entries: Vec<_> = std::fs::read_dir(&s.temp_dir).unwrap().collect();
        assert!(entries.is_empty(), "temp dir not cleaned: {entries:?}");
    }

    #[tokio::test]
    async fn s3_blob_store_open_seekable_removes_private_stage_on_close() {
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::new()).await;
        let (digest, _) = s.put(reader(b"seekable package".to_vec())).await.unwrap();

        let (mut reader, size) = s.open_seekable(&digest).await.unwrap();
        let path = reader.path().to_path_buf();
        let mut value = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut value).unwrap();
        assert_eq!(value.len() as i64, size, "value={value:?} size={size}");
        assert!(path.exists(), "stage missing before close");
        drop(reader);
        assert!(!path.exists(), "stage survived close");
    }

    #[tokio::test]
    async fn s3_blob_store_dedup() {
        let fake = FakeS3::new();
        let objects = fake.clone();
        let (s, _server, _tmp) = new_test_s3_store(fake).await;

        let (d1, _) = s.put(reader(b"same bytes".to_vec())).await.unwrap();
        let (d2, _) = s.put(reader(b"same bytes".to_vec())).await.unwrap();
        assert_eq!(d1, d2, "expected identical digests");
        assert_eq!(objects.len(), 1, "expected 1 stored object after dedup");
    }

    #[tokio::test]
    async fn s3_blob_store_delete() {
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::new()).await;

        let (digest, _) = s.put(reader(b"to delete".to_vec())).await.unwrap();
        s.delete(&digest).await.expect("delete");
        assert!(!s.exists(&digest).await.unwrap(), "blob should be gone");
        // Deleting a missing blob is a no-op.
        s.delete(&digest).await.expect("delete missing");
    }

    #[tokio::test]
    async fn s3_blob_store_open_missing() {
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::new()).await;

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

    #[tokio::test]
    async fn s3_blob_store_walk_digests() {
        // Force the paginator to fetch multiple pages.
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::with_page_size(2)).await;

        let mut want = std::collections::HashSet::new();
        for i in 0..7 {
            let (d, _) = s
                .put(reader(format!("blob-{i}").into_bytes()))
                .await
                .unwrap();
            want.insert(d);
        }

        let mut got: Vec<String> = Vec::new();
        s.walk_digests(&mut |d| {
            got.push(d.to_string());
            Ok(())
        })
        .await
        .expect("walk");

        assert_eq!(got.len(), want.len(), "walked digest count");
        assert!(
            got.windows(2).all(|w| w[0] <= w[1]),
            "digests not in lexicographic order: {got:?}"
        );
        for d in &got {
            assert!(want.contains(d), "unexpected digest {d}");
        }
    }

    #[tokio::test]
    async fn s3_blob_store_walk_digests_fn_error() {
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::new()).await;
        for i in 0..3 {
            s.put(reader(format!("x-{i}").into_bytes())).await.unwrap();
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
        assert_eq!(calls, 1, "fn called {calls} times, want 1");
    }

    #[tokio::test]
    async fn s3_blob_store_walk_digests_cancelled() {
        let (s, _server, _tmp) = new_test_s3_store(FakeS3::new()).await;
        for i in 0..3 {
            s.put(reader(format!("y-{i}").into_bytes())).await.unwrap();
        }

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let mut callback = |_: &str| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };
        let cancelled = tokio::select! {
            biased;
            () = std::future::ready(()) => true,
            _ = s.walk_digests(&mut callback) => false,
        };
        assert!(cancelled, "walk should not outrun cancellation");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "callback ran after cancellation"
        );
    }
}
