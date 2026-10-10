//! Shared HTTP API test harness: builds the router, store and auth service
//! that both the unit tests in `src/api` and the integration tests in `tests/` drive.

// Not every helper is used by every consumer of this harness.
#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt as _;

use crate::auth;
use crate::meta::{self, Permission, Repository, Role, Store, User};
use crate::repo::{Engine, Manager};
use crate::storage::{BlobStore, FsStore};

use crate::api::Handler;

pub const ADMIN_USER: &str = "admin";
pub const ADMIN_PASS: &str = "adminpw";

/// The API under test with the store behind it. The temporary directory is held
/// so the database file outlives the test body.
pub struct TestServer {
    pub app: Router,
    pub store: Arc<Store>,
    pub handler: Arc<Handler>,
    pub authz: Arc<auth::Service>,
    _db_dir: TempDir,
}

/// The harness drives the router directly with `tower::oneshot`, which exercises the same
/// middleware stack without a socket.
pub async fn new_test_server() -> TestServer {
    new_test_server_with(None).await
}

pub async fn new_audit_test_server() -> (TestServer, Arc<crate::audit::Recorder>) {
    let db_dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(db_dir.path().join("api-audit.db"))
            .await
            .expect("open store"),
    );
    let rec = crate::audit::Recorder::new(Arc::clone(&store), &prometheus::Registry::new());
    let srv = build_test_server(
        db_dir,
        store,
        auth::Options {
            session_secret: b"test-secret-test-secret-test-secret".to_vec(),
            ..Default::default()
        },
        Some(Arc::clone(&rec)),
    )
    .await;
    (srv, rec)
}

async fn new_test_server_with(rec: Option<Arc<crate::audit::Recorder>>) -> TestServer {
    let opts = auth::Options {
        session_secret: b"test-secret-test-secret-test-secret".to_vec(),
        ..Default::default()
    };
    let db_dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(db_dir.path().join("api.db"))
            .await
            .expect("open store"),
    );
    build_test_server(db_dir, store, opts, rec).await
}

pub async fn new_console_server() -> TestServer {
    let srv = new_test_server().await;
    srv.handler
        .set_notifier(Arc::new(crate::notify::Notifier::new(
            std::time::Duration::from_secs(2),
        )));
    srv
}

/// Marks the bootstrap admin as the protected admin, matching production wiring
/// where the seeded admin is exempt from lockout and cannot be disabled.
pub async fn new_test_server_protected_admin() -> TestServer {
    new_test_server_opts(auth::Options {
        session_secret: b"test-secret-test-secret-test-secret".to_vec(),
        bootstrap_admin_user: ADMIN_USER.to_string(),
        ..Default::default()
    })
    .await
}

pub async fn new_test_server_opts(opts: auth::Options) -> TestServer {
    let db_dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(db_dir.path().join("api.db"))
            .await
            .expect("open store"),
    );
    build_test_server(db_dir, store, opts, None).await
}

async fn build_test_server(
    db_dir: TempDir,
    store: Arc<Store>,
    opts: auth::Options,
    rec: Option<Arc<crate::audit::Recorder>>,
) -> TestServer {
    auth::set_test_hash_cost();
    crate::server::install_crypto_provider();
    let authz = auth::Service::new(Arc::clone(&store), opts);
    authz
        .bootstrap_admin(ADMIN_USER, ADMIN_PASS)
        .await
        .expect("bootstrap admin");
    let handler = Handler::new(Arc::clone(&store), Some(Arc::clone(&authz)), rec);
    let app = crate::api::routes(Arc::clone(&handler)).layer(axum::middleware::from_fn_with_state(
        Arc::clone(&authz),
        auth::middleware,
    ));
    TestServer {
        app,
        store,
        handler,
        authz,
        _db_dir: db_dir,
    }
}

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl TestResponse {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// The response body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("decode json ({e}): {}", self.text()))
    }

    pub fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
}

impl TestServer {
    pub async fn admin_do(&self, method: Method, uri: &str, body: &str) -> TestResponse {
        self.do_as(ADMIN_USER, ADMIN_PASS, method, uri, body).await
    }

    /// Sends a Basic-auth request as an arbitrary principal.
    pub async fn do_as(
        &self,
        user: &str,
        pass: &str,
        method: Method,
        uri: &str,
        body: &str,
    ) -> TestResponse {
        do_as_on(&self.app, user, pass, method, uri, body).await
    }

    /// Sends an unauthenticated request.
    pub async fn anon_do(&self, method: Method, uri: &str, body: &str) -> TestResponse {
        anon_do_on(&self.app, method, uri, body).await
    }

    pub async fn send(&self, request: Request<Body>) -> TestResponse {
        send_on(&self.app, request).await
    }
}

/// [`TestServer::do_as`] against a router assembled outside the harness, for
/// the test files that wire their own handler stack.
pub async fn do_as_on(
    app: &Router,
    user: &str,
    pass: &str,
    method: Method,
    uri: &str,
    body: &str,
) -> TestResponse {
    let credential =
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}").as_bytes());
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::AUTHORIZATION, format!("Basic {credential}"));
    if !body.is_empty() {
        request = request.header(http::header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(if body.is_empty() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .expect("build request");
    send_on(app, request).await
}

/// [`TestServer::admin_do`] against a router assembled outside the harness.
pub async fn admin_do_on(app: &Router, method: Method, uri: &str, body: &str) -> TestResponse {
    do_as_on(app, ADMIN_USER, ADMIN_PASS, method, uri, body).await
}

/// [`TestServer::anon_do`] against a router assembled outside the harness.
pub async fn anon_do_on(app: &Router, method: Method, uri: &str, body: &str) -> TestResponse {
    let mut request = Request::builder().method(method).uri(uri);
    if !body.is_empty() {
        request = request.header(http::header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(if body.is_empty() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .expect("build request");
    send_on(app, request).await
}

/// Drives one request through a router with `tower::oneshot` and flattens the
/// response.
pub async fn send_on(app: &Router, request: Request<Body>) -> TestResponse {
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

pub fn query_escape(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

pub async fn mk_proxy_repo(srv: &TestServer, name: &str) -> i64 {
    let body = format!(
        r#"{{"name":"{name}","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org"}}"#
    );
    let resp = srv.admin_do(Method::POST, "/repositories", &body).await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "create repo: {}",
        resp.text()
    );
    resp.json()["id"].as_i64().expect("repository id")
}

/// Creates a local user holding a fresh role with one permission.
pub async fn mk_role_user(
    srv: &TestServer,
    username: &str,
    password: &str,
    role_name: &str,
    pattern: &str,
    actions: &str,
) {
    let resp = srv
        .admin_do(
            Method::POST,
            "/users",
            &format!(r#"{{"username":"{username}","password":"{password}"}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let user_id = resp.json()["id"].as_i64().expect("user id");
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            &format!(r#"{{"name":"{role_name}"}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let role_id = resp.json()["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            &format!(r#"{{"repo_pattern":"{pattern}","actions":[{actions}]}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/users/{user_id}/roles"),
            &format!(r#"{{"role_id":{role_id}}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
}

pub struct DanglingHarness {
    pub app: Router,
    pub store: Arc<Store>,
    pub manager: Arc<Manager>,
    pub repository: Repository,
    pub blobs: Arc<dyn BlobStore>,
    _dir: TempDir,
}

pub async fn dangling_harness() -> DanglingHarness {
    // `Engine::new` builds reqwest clients, which need a crypto provider.
    crate::server::install_crypto_provider();
    auth::set_test_hash_cost();
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(dir.path().join("dangling.db"))
            .await
            .expect("open store"),
    );
    let authz = auth::Service::new(
        Arc::clone(&store),
        auth::Options {
            session_secret: b"test-secret-test-secret-test-secret".to_vec(),
            ..Default::default()
        },
    );
    authz
        .bootstrap_admin(ADMIN_USER, ADMIN_PASS)
        .await
        .expect("bootstrap admin");
    let repository = store
        .create_repository(Repository {
            name: "npm-hosted".to_string(),
            format: meta::FORMAT_NPM.to_string(),
            r#type: meta::TYPE_HOSTED.to_string(),
            ..Default::default()
        })
        .await
        .expect("create repository");
    let blobs: Arc<dyn BlobStore> = Arc::new(FsStore::new(dir.path()).expect("blob store"));
    let engine = Engine::new(
        Arc::clone(&store),
        Arc::clone(&blobs),
        &prometheus::Registry::new(),
    );
    let manager = Manager::new(
        engine,
        Arc::clone(&store),
        Some(Arc::clone(&authz)),
        None,
        Some(&prometheus::Registry::new()),
    );
    let recorder = crate::audit::Recorder::new(Arc::clone(&store), &prometheus::Registry::new());
    let handler = Handler::new(Arc::clone(&store), Some(Arc::clone(&authz)), Some(recorder));
    handler.set_repo_manager(Arc::clone(&manager));
    let app = crate::api::routes(Arc::clone(&handler)).layer(axum::middleware::from_fn_with_state(
        Arc::clone(&authz),
        auth::middleware,
    ));
    DanglingHarness {
        app,
        store,
        manager,
        repository,
        blobs,
        _dir: dir,
    }
}

/// Creates a local user with read access to repositories matching `pattern`,
/// which is all these views require.
pub async fn mk_read_user(store: &Store, username: &str, pattern: &str) {
    let hash = auth::hash_password("pw123456").expect("hash password");
    let user = store
        .create_user(User {
            username: username.to_string(),
            password_hash: hash,
            source: meta::SOURCE_LOCAL.to_string(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("create user {username}: {e}"));
    let role = store
        .create_role(Role {
            name: format!("{username}-reader"),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("create role for {username}: {e}"));
    store
        .add_permission(Permission {
            role_id: role.id,
            repo_pattern: pattern.to_string(),
            actions: "read".to_string(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("add permission for {username}: {e}"));
    store
        .assign_role(user.id, role.id)
        .await
        .unwrap_or_else(|e| panic!("assign role to {username}: {e}"));
}

/// Creates a local user with the given CSV actions on every repository, so the
/// lifecycle routes can be exercised as something other than an administrator.
pub async fn mk_upload_user(store: &Store, username: &str, actions: &str) {
    let hash = auth::hash_password("pw123456").expect("hash password");
    let user = store
        .create_user(User {
            username: username.to_string(),
            password_hash: hash,
            source: meta::SOURCE_LOCAL.to_string(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("create user {username}: {e}"));
    let role = store
        .create_role(Role {
            name: format!("{username}-role"),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("create role for {username}: {e}"));
    store
        .add_permission(Permission {
            role_id: role.id,
            repo_pattern: "*".to_string(),
            actions: actions.to_string(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("add permission for {username}: {e}"));
    store
        .assign_role(user.id, role.id)
        .await
        .unwrap_or_else(|e| panic!("assign role to {username}: {e}"));
}
