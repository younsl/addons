//! Shared repository-engine test harness. Crate-internal: it exposes
//! `pub(crate)` repo internals, so it never crosses the crate boundary.

#![allow(dead_code)]

use std::sync::Once;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::{HeaderMap, Method, Request, StatusCode};
use prometheus::Registry;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::meta::{self, Repository, Store};
use crate::repoconfig::Config;
use crate::storage::FsStore;

use crate::repo::{Engine, Manager};

/// A manager, its engine and store, with the temp directories that back them.
pub(crate) struct TestManager {
    pub(crate) manager: Arc<Manager>,
    pub(crate) engine: Arc<Engine>,
    pub(crate) store: Arc<Store>,
    pub(crate) _db_dir: TempDir,
    pub(crate) _blob_dir: TempDir,
}

/// Builds a manager over a fresh temp store and blob directory, with no
/// authorization, no audit recorder and no metric registration.
pub(crate) async fn new_test_manager() -> TestManager {
    new_test_manager_with(None).await
}

/// [`new_test_manager`] with an audit recorder over the same store attached.
pub(crate) async fn new_test_manager_with_recorder() -> (TestManager, Arc<crate::audit::Recorder>) {
    let tm = new_test_manager_with(None).await;
    let rec = crate::audit::Recorder::new(Arc::clone(&tm.store), &prometheus::Registry::new());
    let manager = Manager::new(
        Arc::clone(&tm.engine),
        Arc::clone(&tm.store),
        None,
        Some(Arc::clone(&rec)),
        None,
    );
    (TestManager { manager, ..tm }, rec)
}

/// [`new_test_manager`] with an optional audit recorder attached.
pub(crate) async fn new_test_manager_with(rec: Option<Arc<crate::audit::Recorder>>) -> TestManager {
    // `Engine::new` builds two reqwest clients, and reqwest's
    // `rustls-no-provider` feature panics unless a provider is installed. The
    // server agent does this once at startup; tests do it here.
    crate::server::install_crypto_provider();
    let db_dir = tempfile::tempdir().expect("temp dir");
    let blob_dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(db_dir.path().join("repo.db"))
            .await
            .expect("open store"),
    );
    let blobs = Arc::new(FsStore::new(blob_dir.path()).expect("blob store"));
    let engine = Engine::new(Arc::clone(&store), blobs, &Registry::new());
    let manager = Manager::new(Arc::clone(&engine), Arc::clone(&store), None, rec, None);
    TestManager {
        manager,
        engine,
        store,
        _db_dir: db_dir,
        _blob_dir: blob_dir,
    }
}

/// The format router under test.
pub(crate) fn mux(m: &Arc<Manager>) -> Router {
    m.register(Router::new())
}

pub(crate) async fn mk_repo(
    store: &Arc<Store>,
    name: &str,
    typ: &str,
    upstream: &str,
    cfg: Config,
) -> Repository {
    mk_format_repo(store, name, meta::FORMAT_MAVEN, typ, upstream, cfg).await
}

pub(crate) async fn mk_format_repo(
    store: &Arc<Store>,
    name: &str,
    format: &str,
    typ: &str,
    upstream: &str,
    cfg: Config,
) -> Repository {
    let config_json = cfg.json().expect("config json");
    store
        .create_repository(Repository {
            name: name.to_string(),
            format: format.to_string(),
            r#type: typ.to_string(),
            upstream_url: upstream.to_string(),
            config_json,
            ..Default::default()
        })
        .await
        .expect("create repo")
}

/// A body reader over a fixed byte string, for `Engine::put`.
pub(crate) fn body(s: &str) -> crate::repo::StoreBody {
    Box::pin(std::io::Cursor::new(s.as_bytes().to_vec()))
}

pub(crate) struct TestResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl TestResponse {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub(crate) fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
}

/// Drives one request through a router, mirroring `h.ServeHTTP(rec, req)`.
pub(crate) async fn call(app: &Router, method: Method, uri: &str, body: &str) -> TestResponse {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(if body.is_empty() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .expect("build request");
    send(app, request).await
}

/// Drives a fully built request through a router.
pub(crate) async fn send(app: &Router, request: Request<Body>) -> TestResponse {
    let resp = app
        .clone()
        .oneshot(request)
        .await
        .expect("router is infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    TestResponse {
        status,
        headers,
        body,
    }
}

/// The task is detached; it stops when the test binary exits.
pub(crate) async fn spawn_upstream(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

pub(crate) fn http_time(t: DateTime<Utc>) -> String {
    httpdate::fmt_http_date(t.into())
}

static INIT: Once = Once::new();

/// Lowers the bcrypt cost for the whole test binary. Idempotent.
pub(crate) fn init() {
    INIT.call_once(crate::auth::set_test_hash_cost);
}
