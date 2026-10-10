use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use base64::Engine as _;
use http::{Method, Request, StatusCode};
use tempfile::TempDir;

use forklift::auth;
use forklift::meta::{self, Repository, Store};
use forklift::repo::{Engine, Manager};
use forklift::storage::{BlobStore, FsStore};

use forklift::api::Handler;
use forklift::testing::api::{ADMIN_PASS, ADMIN_USER, TestResponse, send_on};

/// Wires the repository manager (registry engine) behind the management API so
/// the browser raw-upload preflight and PUT can be exercised.
struct RawUploadHarness {
    app: Router,
    repository: Repository,
    _dir: TempDir,
}

async fn raw_upload_harness() -> RawUploadHarness {
    // `Engine::new` builds reqwest clients, which need a crypto provider.
    forklift::server::install_crypto_provider();
    auth::set_test_hash_cost();
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(dir.path().join("raw.db"))
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
            name: "raw-local".to_string(),
            format: meta::FORMAT_RAW.to_string(),
            r#type: meta::TYPE_HOSTED.to_string(),
            ..Default::default()
        })
        .await
        .expect("create repository");
    let blobs: Arc<dyn BlobStore> = Arc::new(FsStore::new(dir.path()).expect("blob store"));
    let engine = Engine::new(Arc::clone(&store), blobs, &prometheus::Registry::new());
    let manager = Manager::new(
        engine,
        Arc::clone(&store),
        Some(Arc::clone(&authz)),
        None,
        Some(&prometheus::Registry::new()),
    );
    let handler = Handler::new(
        Arc::clone(&store),
        Some(Arc::clone(&authz)),
        Some(forklift::audit::Recorder::new(
            Arc::clone(&store),
            &prometheus::Registry::new(),
        )),
    );
    handler.set_repo_manager(manager);
    let app = forklift::api::routes(Arc::clone(&handler)).layer(
        axum::middleware::from_fn_with_state(Arc::clone(&authz), auth::middleware),
    );
    RawUploadHarness {
        app,
        repository,
        _dir: dir,
    }
}

/// The raw upload carries a body of its own content type, which the shared `do_as` helper would
/// override with `application/json`.
async fn admin_body(
    app: &Router,
    method: Method,
    uri: &str,
    content_type: &str,
    body: &'static [u8],
) -> TestResponse {
    let credential = base64::engine::general_purpose::STANDARD
        .encode(format!("{ADMIN_USER}:{ADMIN_PASS}").as_bytes());
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .expect("build request");
    send_on(app, request).await
}

#[tokio::test]
async fn raw_upload_validate_and_put() {
    let h = raw_upload_harness().await;
    let base = format!("/repositories/{}", h.repository.id);

    // Preflight the target path.
    let resp = admin_body(
        &h.app,
        Method::POST,
        &format!("{base}/artifacts/validate-upload"),
        "application/json",
        br#"{"path":"dir/file.bin","size":3,"content_type":"application/octet-stream"}"#,
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "validate = {} {}",
        resp.status,
        resp.text()
    );

    // Stream the bytes in.
    let resp = admin_body(
        &h.app,
        Method::PUT,
        &format!("{base}/artifacts/upload?path=dir/file.bin"),
        "application/octet-stream",
        b"abc",
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "put = {} {}",
        resp.status,
        resp.text()
    );

    // A second preflight on the now-occupied path reports the collision.
    let resp = admin_body(
        &h.app,
        Method::POST,
        &format!("{base}/artifacts/validate-upload"),
        "application/json",
        br#"{"path":"dir/file.bin","size":3,"content_type":"application/octet-stream"}"#,
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "re-validate = {} {}",
        resp.status,
        resp.text()
    );
    assert!(
        resp.text().contains(r#""exists":true"#),
        "expected exists=true, got {}",
        resp.text()
    );
}

#[tokio::test]
async fn raw_upload_rejects_bad_json() {
    let h = raw_upload_harness().await;
    let resp = admin_body(
        &h.app,
        Method::POST,
        &format!(
            "/repositories/{}/artifacts/validate-upload",
            h.repository.id
        ),
        "application/json",
        b"not json",
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "bad json = {} {}",
        resp.status,
        resp.text()
    );
}

#[tokio::test]
async fn raw_upload_error_paths() {
    let h = raw_upload_harness().await;
    let base = format!("/repositories/{}", h.repository.id);

    // A path traversal is refused by `validate_hosted_upload` -> `map_upload_error`.
    let resp = admin_body(
        &h.app,
        Method::PUT,
        &format!("{base}/artifacts/upload?path=../../etc/passwd"),
        "application/octet-stream",
        b"x",
    )
    .await;
    assert!(
        resp.status.is_client_error(),
        "traversal PUT = {}, want 4xx",
        resp.status
    );

    // Preflight of the same bad path is refused too.
    let resp = admin_body(
        &h.app,
        Method::POST,
        &format!("{base}/artifacts/validate-upload"),
        "application/json",
        br#"{"path":"../../etc/passwd","size":1,"content_type":"text/plain"}"#,
    )
    .await;
    assert!(
        resp.status.is_client_error(),
        "traversal validate = {}, want 4xx",
        resp.status
    );
}
