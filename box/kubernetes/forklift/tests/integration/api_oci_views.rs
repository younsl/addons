use std::sync::Arc;

use axum::Router;
use http::{Method, StatusCode};
use serde_json::Value;
use tempfile::TempDir;

use forklift::auth;
use forklift::meta::{self, Artifact, ArtifactLabel, Repository, Store};
use forklift::repo::{Engine, Manager, oci_manifest_path};
use forklift::storage::{BlobStore, FsStore};

use forklift::api::Handler;
use forklift::testing::api::{ADMIN_PASS, ADMIN_USER, do_as_on, mk_read_user, query_escape};

/// Wires the console's OCI read surfaces behind a repository manager with a
/// real blob store, which both of them need: the views read the stored manifest
/// document to classify the artifact and size it.
struct OciViewHarness {
    app: Router,
    store: Arc<Store>,
    blobs: Arc<dyn BlobStore>,
    repository: Repository,
    _dir: TempDir,
}

async fn oci_view_harness() -> OciViewHarness {
    // `Engine::new` builds reqwest clients, which need a crypto provider.
    forklift::server::install_crypto_provider();
    auth::set_test_hash_cost();
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Arc::new(
        Store::open(dir.path().join("api-oci.db"))
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
            name: "oci-hosted".to_string(),
            format: meta::FORMAT_OCI.to_string(),
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
    OciViewHarness {
        app,
        store,
        blobs,
        repository,
        _dir: dir,
    }
}

/// Stores one image manifest with its config and layer blobs and tags it, which
/// is the state a push leaves behind.
async fn seed_oci_image(
    store: &Store,
    blobs: &Arc<dyn BlobStore>,
    repo: &Repository,
    name: &str,
    tag: &str,
    pushed_by: &str,
) -> String {
    // A push stores each blob and records an artifact row for it under
    // "<name>/blobs/<digest>"; the detail view resolves the config through that
    // row, so the seed has to create it too.
    async fn put(
        store: &Store,
        blobs: &Arc<dyn BlobStore>,
        repo_id: i64,
        name: &str,
        pushed_by: &str,
        body: &str,
    ) -> (String, i64) {
        let (digest, size) = blobs
            .put(Box::pin(std::io::Cursor::new(body.as_bytes().to_vec())))
            .await
            .expect("put blob");
        store
            .put_artifact(Artifact {
                repo_id,
                path: format!("{name}/blobs/sha256:{digest}"),
                blob_sha256: digest.clone(),
                size,
                content_type: "application/octet-stream".to_string(),
                cached_by: pushed_by.to_string(),
                ..Default::default()
            })
            .await
            .expect("put blob artifact");
        (format!("sha256:{digest}"), size)
    }
    let (config_digest, config_size) = put(
        store,
        blobs,
        repo.id,
        name,
        pushed_by,
        r#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#,
    )
    .await;
    let (layer_digest, layer_size) =
        put(store, blobs, repo.id, name, pushed_by, "layer-bytes").await;
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_digest}","size":{config_size}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{layer_digest}","size":{layer_size}}}]}}"#
    );
    let (manifest_digest_hex, manifest_size) = blobs
        .put(Box::pin(std::io::Cursor::new(manifest.into_bytes())))
        .await
        .expect("put manifest");
    let manifest_digest = format!("sha256:{manifest_digest_hex}");
    store
        .put_artifact(Artifact {
            repo_id: repo.id,
            path: oci_manifest_path(name, &manifest_digest),
            blob_sha256: manifest_digest_hex,
            size: manifest_size,
            content_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
            cached_by: pushed_by.to_string(),
            ..Default::default()
        })
        .await
        .expect("put manifest artifact");
    store
        .upsert_oci_tag(repo.id, name, tag, &manifest_digest)
        .await
        .expect("tag manifest");
    manifest_digest
}

/// The image listing is the OCI equivalent of the artifacts table, so it must
/// carry the same label affordance: the manifest's stored path (which the label
/// endpoints take), its labels, and whether this caller may change them. The
/// pusher may, a reader who did not push may not.
#[tokio::test]
async fn list_oci_tags_carries_labels_and_permission() {
    let h = oci_view_harness().await;
    let digest = seed_oci_image(
        &h.store,
        &h.blobs,
        &h.repository,
        "library/nginx",
        "1.27",
        "bob",
    )
    .await;
    let path = oci_manifest_path("library/nginx", &digest);
    h.store
        .add_artifact_label(ArtifactLabel {
            repo_id: h.repository.id,
            path: path.clone(),
            label: "team:Payments".to_string(),
            created_by: "bob".to_string(),
            ..Default::default()
        })
        .await
        .expect("add label");

    let tags = async |user: &str, pass: &str| -> Vec<Value> {
        let resp = do_as_on(
            &h.app,
            user,
            pass,
            Method::GET,
            &format!("/repositories/{}/oci-tags", h.repository.id),
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "oci-tags = {} {}",
            resp.status,
            resp.text()
        );
        resp.json()["tags"].as_array().cloned().unwrap_or_default()
    };

    let got = tags(ADMIN_USER, ADMIN_PASS).await;
    assert_eq!(got.len(), 1, "tags = {got:?}, want 1");
    let row = &got[0];
    assert!(
        row["name"] == "library/nginx" && row["tag"] == "1.27" && row["digest"] == digest,
        "row identity = {row}"
    );
    assert!(
        row["kind"] == "image" && row["size"].as_i64() != Some(0),
        "row summary = {row}, want an image with a non-zero size"
    );
    assert_eq!(
        row["path"], path,
        "row path = {}, want the stored manifest path {path:?}",
        row["path"]
    );
    assert!(
        row["labels"].as_array().map(Vec::len) == Some(1)
            && row["labels"][0]["label"] == "team:Payments"
            && row["can_label"] == true,
        "row labels = {} can_label={}",
        row["labels"],
        row["can_label"]
    );

    // The pusher may label their own image; a reader who did not push may not.
    mk_read_user(&h.store, "bob", "oci-*").await;
    mk_read_user(&h.store, "eve", "oci-*").await;
    assert_eq!(
        tags("bob", "pw123456").await[0]["can_label"],
        true,
        "pusher can_label = false"
    );
    assert_ne!(
        tags("eve", "pw123456").await[0]["can_label"],
        true,
        "non-pusher can_label = true"
    );
    assert_eq!(
        tags("eve", "pw123456").await[0]["labels"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "non-pusher cannot see the labels"
    );
}

/// The drill-down answers with the same label fields as the listing, plus the
/// manifest path, so the detail page can offer the editor without rebuilding the
/// identity from name and digest. Its two argument errors are pinned too: both
/// name and ref are required, and an unknown reference is a 404 rather than an
/// empty page.
#[tokio::test]
async fn get_oci_detail_labels_and_argument_errors() {
    let h = oci_view_harness().await;
    let digest = seed_oci_image(
        &h.store,
        &h.blobs,
        &h.repository,
        "library/nginx",
        "1.27",
        ADMIN_USER,
    )
    .await;
    let path = oci_manifest_path("library/nginx", &digest);
    h.store
        .add_artifact_label(ArtifactLabel {
            repo_id: h.repository.id,
            path: path.clone(),
            label: "keep-forever".to_string(),
            created_by: ADMIN_USER.to_string(),
            ..Default::default()
        })
        .await
        .expect("add label");

    let do_query = async |query: &str| {
        do_as_on(
            &h.app,
            ADMIN_USER,
            ADMIN_PASS,
            Method::GET,
            &format!("/repositories/{}/oci-detail?{query}", h.repository.id),
            "",
        )
        .await
    };

    let resp = do_query("name=library/nginx&ref=1.27").await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "detail by tag = {} {}",
        resp.status,
        resp.text()
    );
    let detail = resp.json();
    assert!(
        detail["info"]["tag"] == "1.27"
            && detail["info"]["digest"] == digest
            && detail["path"] == path,
        "detail identity = {} path={}",
        detail["info"],
        detail["path"]
    );
    assert!(
        detail["labels"].as_array().map(Vec::len) == Some(1)
            && detail["labels"][0]["label"] == "keep-forever"
            && detail["can_label"] == true,
        "detail labels = {} can_label={}",
        detail["labels"],
        detail["can_label"]
    );
    assert!(
        detail["image"]["architecture"] == "amd64" && detail["image"]["os"] == "linux",
        "image summary = {}",
        detail["image"]
    );
    assert!(
        !detail["manifest_json"].is_null() && !detail["config_json"].is_null(),
        "detail is missing the raw manifest or config document"
    );

    // A digest reference resolves the same artifact, with no tag attached.
    let resp = do_query(&format!("name=library/nginx&ref={}", query_escape(&digest))).await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "detail by digest = {} {}",
        resp.status,
        resp.text()
    );
    let detail = resp.json();
    assert!(
        detail["info"]["tag"] == "" && detail["path"] == path,
        "digest reference = {} path={}",
        detail["info"],
        detail["path"]
    );

    for query in ["", "name=library/nginx", "ref=1.27"] {
        let resp = do_query(query).await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "detail with {query:?} = {}, want 400",
            resp.status
        );
    }
    let resp = do_query("name=library/nginx&ref=absent").await;
    assert_eq!(
        resp.status,
        StatusCode::NOT_FOUND,
        "unknown reference = {}, want 404",
        resp.status
    );
}

/// Both OCI views are ordinary repository reads, so a principal without read
/// access to the repository is refused rather than shown an empty list.
#[tokio::test]
async fn oci_views_require_repository_read() {
    let h = oci_view_harness().await;
    seed_oci_image(
        &h.store,
        &h.blobs,
        &h.repository,
        "library/nginx",
        "1.27",
        ADMIN_USER,
    )
    .await;
    // A user whose role covers a different repository only.
    mk_read_user(&h.store, "eve", "npm-*").await;

    for path in ["/oci-tags", "/oci-detail?name=library/nginx&ref=1.27"] {
        let resp = do_as_on(
            &h.app,
            "eve",
            "pw123456",
            Method::GET,
            &format!("/repositories/{}{path}", h.repository.id),
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "{path} as a non-reader = {}, want 403",
            resp.status
        );
    }
}
