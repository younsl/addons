//! Shared auth-service test harness. Crate-internal: it reaches into
//! `auth::authz` and `auth::middleware`, which are private to the crate.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use base64::Engine as _;
use http::Request;

use crate::auth::*;
use crate::meta;

/// A store plus the temp dir that owns its file, and a Service over it.
pub(crate) struct TestService {
    pub svc: Arc<Service>,
    pub store: Arc<meta::Store>,
    _dir: tempfile::TempDir,
}

pub(crate) async fn new_test_service() -> TestService {
    crate::testing::auth::init();
    new_test_service_with(Options {
        session_secret: b"test-secret-test-secret-test-secret".to_vec(),
        ..Default::default()
    })
    .await
}

pub(crate) async fn new_test_service_with(opts: Options) -> TestService {
    crate::testing::auth::init();
    let (store, dir) = crate::testing::meta::test_store().await;
    let store = Arc::new(store);
    let svc = Service::new(Arc::clone(&store), opts);
    TestService {
        svc,
        store,
        _dir: dir,
    }
}

/// An empty `GET /` request's parts, the Rust spelling of
/// `httptest.NewRequest(http.MethodGet, "/", nil)`.
pub(crate) fn request_parts() -> http::request::Parts {
    Request::builder()
        .uri("/")
        .body(Body::empty())
        .expect("build request")
        .into_parts()
        .0
}

/// `r.SetBasicAuth(user, pass)`.
pub(crate) fn basic_auth_parts(user: &str, pass: &str) -> http::request::Parts {
    let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    Request::builder()
        .uri("/")
        .header(http::header::AUTHORIZATION, format!("Basic {raw}"))
        .body(Body::empty())
        .expect("build request")
        .into_parts()
        .0
}

/// `r.AddCookie(&http.Cookie{Name: name, Value: value})`.
pub(crate) fn cookie_parts(name: &str, value: &str) -> http::request::Parts {
    Request::builder()
        .uri("/")
        .header(http::header::COOKIE, format!("{name}={value}"))
        .body(Body::empty())
        .expect("build request")
        .into_parts()
        .0
}

/// Rebuilds a request from parts so it can be routed.
pub(crate) fn to_request(parts: http::request::Parts) -> Request<Body> {
    Request::from_parts(parts, Body::empty())
}

use std::sync::Once;

static INIT: Once = Once::new();

/// Lowers the bcrypt cost for the whole test binary. Idempotent.
pub(crate) fn init() {
    INIT.call_once(crate::auth::set_test_hash_cost);
}
