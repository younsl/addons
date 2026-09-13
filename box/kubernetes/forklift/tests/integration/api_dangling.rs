use axum::Router;
use http::{Method, StatusCode};
use serde_json::Value;

use forklift::meta::{self, Artifact};

use forklift::testing::api::{ADMIN_USER, admin_do_on, dangling_harness, query_escape};

/// Wires the artifact listing behind the repository manager so the
/// broken-artifact annotation can be exercised end to end.
async fn list_artifacts_for_test(app: &Router, repo_id: i64) -> Vec<Value> {
    let resp = admin_do_on(
        app,
        Method::GET,
        &format!("/repositories/{repo_id}/artifacts"),
        "",
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::OK,
        "list artifacts = {} {}",
        resp.status,
        resp.text()
    );
    resp.json()["artifacts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Verifies the artifacts view marks an artifact whose bytes are gone, so a
/// user sees it in the browser instead of discovering it as a failed install.
#[tokio::test]
async fn artifact_list_flags_missing_blob() {
    let h = dangling_harness().await;

    let art = h
        .store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit".to_string(),
            blob_sha256: "sha-gone".to_string(),
            size: 3698,
            content_type: "application/json".to_string(),
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");

    // Nothing observed yet: the listing must not claim a problem it cannot know.
    for dto in list_artifacts_for_test(&h.app, h.repository.id).await {
        assert_ne!(
            dto["blob_missing"], true,
            "artifact flagged before any missing blob was observed"
        );
    }

    h.manager.record_dangling_ref(
        h.repository.id,
        &art.path,
        &art.blob_sha256,
        &art.artifact_role,
        500,
    );

    let arts = list_artifacts_for_test(&h.app, h.repository.id).await;
    assert_eq!(arts.len(), 1, "artifacts = {}, want 1", arts.len());
    assert_eq!(
        arts[0]["blob_missing"], true,
        "artifact with missing bytes is not flagged"
    );
    assert!(
        arts[0]["blob_missing_since"].is_string(),
        "blob_missing_since not reported: {}",
        arts[0]
    );
}

/// Verifies a fixed artifact stops being flagged: the recorded failure names a
/// digest the artifact no longer points at.
#[tokio::test]
async fn artifact_list_clears_flag_after_republish() {
    let h = dangling_harness().await;

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
    let arts = list_artifacts_for_test(&h.app, h.repository.id).await;
    assert_eq!(
        arts[0]["blob_missing"], true,
        "precondition: artifact should be flagged"
    );

    // A republish rewrites the artifact to new bytes.
    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit".to_string(),
            blob_sha256: "sha-fresh".to_string(),
            size: 12,
            content_type: "application/json".to_string(),
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("republish artifact");

    let arts = list_artifacts_for_test(&h.app, h.repository.id).await;
    assert_ne!(
        arts[0]["blob_missing"], true,
        "republished artifact is still flagged as broken"
    );
    let refs = h.manager.dangling_refs_for_repo(h.repository.id);
    assert!(
        refs.is_empty(),
        "stale reference kept after republish: {refs:?}"
    );
}

async fn force_delete_for_test(app: &Router, repo_id: i64, path: &str) -> (StatusCode, String) {
    let resp = admin_do_on(
        app,
        Method::DELETE,
        &format!(
            "/repositories/{repo_id}/artifacts?force=true&path={}",
            query_escape(path)
        ),
        "",
    )
    .await;
    (resp.status, resp.text())
}

/// The repair path: an artifact owned by a managed publication, whose bytes are
/// gone, can be removed. The ordinary delete refuses it and the publication
/// lifecycle cannot run, so without this the repository is stuck with an
/// artifact it can neither serve nor delete.
#[tokio::test]
async fn force_delete_removes_broken_managed_artifact() {
    let h = dangling_harness().await;

    // Publications are normally created by the upload planner; insert the row
    // directly so the test can start from a managed artifact whose bytes are
    // gone.
    let repo_id = h.repository.id;
    h.store
        .write(move |c| {
            c.execute(
                "INSERT INTO artifact_publications(id, repo_id, format, package_name, version, coordinate,
		 upload_id, created_by, created_by_source, yanked, created_at, updated_at)
		 VALUES('pub-1', ?, 'npm', '@msp/toolkit', '0.6.0', '@msp/toolkit@0.6.0', 'upload-1', ?, 'test', 0,
		 '2026-08-07T00:00:00Z', '2026-08-07T00:00:00Z')",
                rusqlite::params![repo_id, ADMIN_USER],
            )
            .map_err(|e| meta::Error::sqlite("publication fixture", e))
        })
        .await
        .expect("publication fixture");
    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit/-/toolkit-0.6.0.tgz".to_string(),
            version: "0.6.0".to_string(),
            blob_sha256: "sha-gone".to_string(),
            size: 10,
            artifact_role: "primary".to_string(),
            publication_id: "pub-1".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");

    // The ordinary delete must still refuse it.
    let resp = admin_do_on(
        &h.app,
        Method::DELETE,
        &format!(
            "/repositories/{}/artifacts?path={}",
            h.repository.id,
            query_escape("@msp/toolkit/-/toolkit-0.6.0.tgz")
        ),
        "",
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "plain delete = {}, want 409",
        resp.status
    );

    let (code, body) =
        force_delete_for_test(&h.app, h.repository.id, "@msp/toolkit/-/toolkit-0.6.0.tgz").await;
    assert_eq!(code, StatusCode::OK, "force delete = {code} {body}");
    assert!(
        body.contains(r#""publication_deleted":true"#),
        "publication not removed with its last asset: {body}"
    );
    let err = h
        .store
        .get_artifact(h.repository.id, "@msp/toolkit/-/toolkit-0.6.0.tgz")
        .await
        .err();
    assert!(
        matches!(err, Some(meta::Error::NotFound)),
        "artifact still present after force delete: {err:?}"
    );
    // No tombstone: the whole point is to let the coordinate be published again.
    let tombstoned = h
        .store
        .has_publication_tombstone(
            h.repository.id,
            meta::FORMAT_NPM,
            "@msp/toolkit",
            "0.6.0",
            "*",
        )
        .await
        .expect("tombstone lookup");
    assert!(
        !tombstoned,
        "force delete wrote a tombstone, blocking republish of the same version"
    );
    let refs = h.manager.dangling_refs_for_repo(h.repository.id);
    assert!(
        refs.is_empty(),
        "dangling entry kept after the artifact was removed: {refs:?}"
    );
}

/// Keeps the escape hatch narrow: present bytes mean the managed guard still
/// applies.
#[tokio::test]
async fn force_delete_refuses_healthy_artifact() {
    let h = dangling_harness().await;

    let (digest, _) = h
        .blobs
        .put(Box::pin(std::io::Cursor::new(b"real bytes".to_vec())))
        .await
        .expect("put blob");
    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit".to_string(),
            blob_sha256: digest,
            size: 10,
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");

    let (code, body) = force_delete_for_test(&h.app, h.repository.id, "@msp/toolkit").await;
    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "force delete of a healthy artifact = {code} {body}, want 409"
    );
    h.store
        .get_artifact(h.repository.id, "@msp/toolkit")
        .await
        .expect("healthy artifact was removed");
}

/// Keeps force from becoming a repository-wide bypass of the managed-artifact
/// guard.
#[tokio::test]
async fn force_delete_requires_path() {
    let h = dangling_harness().await;
    let resp = admin_do_on(
        &h.app,
        Method::DELETE,
        &format!("/repositories/{}/artifacts?force=true", h.repository.id),
        "",
    )
    .await;
    assert_eq!(
        resp.status,
        StatusCode::BAD_REQUEST,
        "force purge = {}, want 400",
        resp.status
    );
}

/// Covers repair in place: an operator puts the object back at the same digest.
/// The recorded failure then names bytes that exist, so the warning has to clear
/// even though nothing about the artifact row changed.
#[tokio::test]
async fn artifact_list_clears_flag_after_bytes_restored() {
    let h = dangling_harness().await;

    let (digest, _) = h
        .blobs
        .put(Box::pin(std::io::Cursor::new(b"{}".to_vec())))
        .await
        .expect("put blob");
    h.store
        .put_artifact(Artifact {
            repo_id: h.repository.id,
            path: "@msp/toolkit".to_string(),
            blob_sha256: digest.clone(),
            size: 2,
            content_type: "application/json".to_string(),
            artifact_role: "index".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");
    // The failure was recorded while the bytes were gone; they are back now.
    h.manager
        .record_dangling_ref(h.repository.id, "@msp/toolkit", &digest, "index", 500);

    let arts = list_artifacts_for_test(&h.app, h.repository.id).await;
    assert_ne!(
        arts[0]["blob_missing"], true,
        "artifact still flagged after its bytes were restored at the same digest"
    );
    let refs = h.manager.dangling_refs_for_repo(h.repository.id);
    assert!(
        refs.is_empty(),
        "stale reference kept after repair: {refs:?}"
    );
}
