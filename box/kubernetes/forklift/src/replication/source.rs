//! The leader side of PV-based replication: a token-gated SQLite snapshot endpoint plus a
//! cursor-paged blob listing and blob fetch.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::TryStreamExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::io::ReaderStream;

use crate::server::http_error;
use crate::{meta, storage};

/// Caps one blob digest listing response.
pub(crate) const DEFAULT_PAGE_SIZE: i64 = 1000;

/// Serves the leader-side replication endpoints. The endpoints expose the full
/// database (including credential hashes), so they are guarded by a shared
/// bearer token and must not be exposed outside the cluster.
pub struct Source {
    store: Arc<meta::Store>,
    blobs: Arc<dyn storage::BlobStore>,
    token: String,
    data_dir: PathBuf,

    /// Serializes snapshot generation; there is only one standby.
    ///
    snapshot_mu: Arc<Mutex<()>>,
}

impl Source {
    /// Builds the leader-side handler set.
    pub fn new(
        store: Arc<meta::Store>,
        blobs: Arc<dyn storage::BlobStore>,
        token: &str,
        data_dir: impl Into<PathBuf>,
    ) -> Arc<Source> {
        Arc::new(Source {
            store,
            blobs,
            token: token.to_string(),
            data_dir: data_dir.into(),
            snapshot_mu: Arc::new(Mutex::new(())),
        })
    }

    /// Returns the replication endpoints, all gated by the shared token.
    pub fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/db", get(handle_db))
            .route("/blobs", get(handle_list_blobs))
            .route("/blobs/{digest}", get(handle_get_blob))
            // `layer` (not `route_layer`) so the token check also covers
            // unmatched paths under the mount, the way a chi `Use` middleware
            // ran ahead of the router's NotFound handler.
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(self),
                require_token,
            ))
            .with_state(Arc::clone(self))
    }
}

/// Rejects any request whose `Authorization` header is not the configured
/// bearer token. An empty configured token disables the endpoints outright
/// rather than allowing everything through.
async fn require_token(State(s): State<Arc<Source>>, req: Request, next: Next) -> Response {
    let got = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let want = format!("Bearer {}", s.token);
    if s.token.is_empty() || got.as_bytes().ct_eq(want.as_bytes()).unwrap_u8() != 1 {
        return http_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}

struct SnapshotHold {
    _guard: OwnedMutexGuard<()>,
    path: PathBuf,
}

impl Drop for SnapshotHold {
    fn drop(&mut self) {
        // A single unlink; cheap enough to run inline rather than paying for a
        // spawned blocking task on every snapshot request.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Streams a consistent point-in-time SQLite snapshot (`VACUUM INTO`), which is
/// a single clean database file with no WAL sidecars.
async fn handle_db(State(s): State<Arc<Source>>) -> Response {
    let guard = Arc::clone(&s.snapshot_mu).lock_owned().await;

    let dir = s.data_dir.join("replication");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::error!(err = %e, "replication: create snapshot dir");
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed");
    }
    let path = dir.join("snapshot.db");
    let hold = SnapshotHold {
        _guard: guard,
        path: path.clone(),
    };

    if let Err(e) = s.store.snapshot(&path).await {
        tracing::error!(err = %e, "replication: snapshot");
        return http_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed");
    }
    let f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(err = %e, "replication: open snapshot");
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed");
        }
    };
    let size = match f.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::error!(err = %e, "replication: stat snapshot");
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed");
        }
    };

    let stream = ReaderStream::new(f).inspect_err(move |e| {
        // Captured so the lock and the temp file outlive the body.
        let _ = &hold;
        tracing::warn!(err = %e, "replication: stream snapshot");
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// The digest listing response.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BlobPage {
    #[serde(
        serialize_with = "serialize_optional_list",
        deserialize_with = "deserialize_optional_list"
    )]
    pub(crate) digests: Vec<String>,
}

fn serialize_optional_list<S: Serializer>(v: &[String], s: S) -> Result<S::Ok, S::Error> {
    if v.is_empty() {
        s.serialize_none()
    } else {
        s.collect_seq(v)
    }
}

fn deserialize_optional_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

fn query_get(raw: Option<&str>, key: &str) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    for (k, v) in form_urlencoded::parse(raw.as_bytes()) {
        if k == key {
            return v.into_owned();
        }
    }
    String::new()
}

/// Returns blob digests ordered by sha256, strictly after the `after` cursor.
/// An empty page means the listing is complete.
async fn handle_list_blobs(
    State(s): State<Arc<Source>>,
    axum::extract::RawQuery(raw): axum::extract::RawQuery,
) -> Response {
    let after = query_get(raw.as_deref(), "after");
    let mut limit = DEFAULT_PAGE_SIZE;
    let v = query_get(raw.as_deref(), "limit");
    if !v.is_empty() {
        match v.parse::<i64>() {
            Ok(n) if n > 0 && n <= DEFAULT_PAGE_SIZE => limit = n,
            _ => return http_error(StatusCode::BAD_REQUEST, "invalid limit"),
        }
    }
    let digests = match s.store.list_blob_digests(&after, limit).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(err = %e, "replication: list blobs");
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "list failed");
        }
    };
    // `json.Encoder.Encode` terminated the object with a newline.
    let body = match serde_json::to_string(&BlobPage { digests }) {
        Ok(mut b) => {
            b.push('\n');
            b
        }
        Err(e) => {
            tracing::error!(err = %e, "replication: list blobs");
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "list failed");
        }
    };
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Streams one content-addressed blob by digest.
async fn handle_get_blob(State(s): State<Arc<Source>>, Path(digest): Path<String>) -> Response {
    let (rc, size) = match s.blobs.open(&digest).await {
        Ok(v) => v,
        Err(storage::Error::NotFound) => {
            return http_error(StatusCode::NOT_FOUND, "not found");
        }
        Err(e) => {
            tracing::error!(digest = %digest, err = %e, "replication: open blob");
            return http_error(StatusCode::INTERNAL_SERVER_ERROR, "open failed");
        }
    };
    let stream = ReaderStream::new(rc).inspect_err(move |e| {
        tracing::warn!(digest = %digest, err = %e, "replication: stream blob");
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
