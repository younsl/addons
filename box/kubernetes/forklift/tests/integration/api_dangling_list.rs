use axum::Router;
use chrono::{DateTime, Utc};
use http::{Method, StatusCode};
use serde_json::Value;

use forklift::meta::Artifact;

use forklift::testing::api::{ADMIN_PASS, ADMIN_USER, dangling_harness, do_as_on, mk_read_user};

async fn list_dangling_for_test(
    app: &Router,
    repo_id: i64,
    user: &str,
    pass: &str,
) -> (Vec<Value>, StatusCode) {
    let resp = do_as_on(
        app,
        user,
        pass,
        Method::GET,
        &format!("/repositories/{repo_id}/dangling"),
        "",
    )
    .await;
    if resp.status != StatusCode::OK {
        return (Vec::new(), resp.status);
    }
    let refs = resp.json();
    let refs = refs
        .as_array()
        .unwrap_or_else(|| panic!("decode dangling listing: {}", resp.text()))
        .clone();
    (refs, resp.status)
}

fn timestamp(value: &Value) -> DateTime<Utc> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| panic!("timestamp: {value}"))
}

/// The per-repository broken-artifact listing is what the audit log marks its
/// rows from, so it has to report the failures that are still real and drop the
/// ones that are not. Three states matter: a live failure is listed with the
/// detail an operator needs, a reference whose artifact was deleted is forgotten
/// rather than shown, and bytes restored at the same digest clear the warning.
#[tokio::test]
async fn list_repo_dangling_reports_and_prunes() {
    let h = dangling_harness().await;

    // A repository with no observed failures answers with an empty list, not
    // null, so the console can render it without a guard.
    let (refs, code) =
        list_dangling_for_test(&h.app, h.repository.id, ADMIN_USER, ADMIN_PASS).await;
    assert!(
        code == StatusCode::OK && refs.is_empty(),
        "clean repository = {code} {refs:?}, want 200 and an empty list"
    );

    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit".to_string(),
            blob_sha256: "sha-gone".to_string(),
            size: 10,
            content_type: "application/json".to_string(),
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");
    h.manager
        .record_dangling_ref(h.repository.id, "@msp/toolkit", "sha-gone", "index", 500);
    h.manager
        .record_dangling_ref(h.repository.id, "@msp/toolkit", "sha-gone", "index", 503);

    let (refs, _) = list_dangling_for_test(&h.app, h.repository.id, ADMIN_USER, ADMIN_PASS).await;
    assert_eq!(refs.len(), 1, "refs = {refs:?}, want 1");
    let reference = &refs[0];
    assert!(
        reference["repository"] == h.repository.name.as_str()
            && reference["repo_id"] == h.repository.id
            && reference["path"] == "@msp/toolkit",
        "ref identity = {reference}"
    );
    assert!(
        reference["sha256"] == "sha-gone" && reference["role"] == "index" && reference["hits"] == 2,
        "ref detail = {reference}, want both failures counted"
    );
    // The status breakdown is what lets an operator match the row against the
    // error a user reported, so it must carry both codes seen.
    assert!(
        reference["statuses"].as_array().map(Vec::len) == Some(2)
            && reference["last_status"] == 503,
        "ref statuses = {} last={}",
        reference["statuses"],
        reference["last_status"]
    );
    assert!(
        timestamp(&reference["last_seen"]) >= timestamp(&reference["first_seen"]),
        "ref timestamps = {reference}"
    );

    // Restoring the bytes at the same digest clears the warning: the digest is
    // unchanged, so only the blob store can answer whether it is still broken.
    let (digest, _) = h
        .blobs
        .put(Box::pin(std::io::Cursor::new(b"restored".to_vec())))
        .await
        .expect("put blob");
    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/restored".to_string(),
            blob_sha256: digest.clone(),
            size: 8,
            content_type: "application/json".to_string(),
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("put restored artifact");
    h.manager
        .record_dangling_ref(h.repository.id, "@msp/restored", &digest, "index", 500);
    let (refs, _) = list_dangling_for_test(&h.app, h.repository.id, ADMIN_USER, ADMIN_PASS).await;
    for r in &refs {
        assert_ne!(
            r["path"], "@msp/restored",
            "artifact whose bytes are present is still reported: {r}"
        );
    }

    // Deleting the artifact forgets the reference rather than reporting a path
    // that is no longer stored.
    h.store
        .delete_artifact(h.repository.id, "@msp/toolkit")
        .await
        .expect("delete artifact");
    let (refs, _) = list_dangling_for_test(&h.app, h.repository.id, ADMIN_USER, ADMIN_PASS).await;
    assert!(refs.is_empty(), "refs after delete = {refs:?}, want none");
    let tracked = h.manager.dangling_refs_for_repo(h.repository.id);
    assert!(
        tracked.is_empty(),
        "stale reference kept after the artifact was deleted: {tracked:?}"
    );
}

/// The listing names artifact paths in a repository, so it follows repository
/// read access like every other browse surface.
#[tokio::test]
async fn list_repo_dangling_requires_read() {
    let h = dangling_harness().await;
    mk_read_user(&h.store, "eve", "maven-*").await;
    let (_, code) = list_dangling_for_test(&h.app, h.repository.id, "eve", "pw123456").await;
    assert_eq!(code, StatusCode::FORBIDDEN, "non-reader = {code}, want 403");
    let (_, code) = list_dangling_for_test(&h.app, 4242, ADMIN_USER, ADMIN_PASS).await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "unknown repository = {code}, want 404"
    );
}
