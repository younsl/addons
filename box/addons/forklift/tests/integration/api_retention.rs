use chrono::{TimeDelta, Utc};
use http::{Method, StatusCode};

use forklift::meta::Artifact;
use forklift::testing::api::{admin_do_on, dangling_harness};

#[tokio::test]
async fn retention_preview_counts_idle_artifacts() {
    let h = dangling_harness().await;
    let now = Utc::now();
    for (path, idle_days, size) in [
        ("old-1.0.0.tgz", 40, 10),
        ("mid-1.0.0.tgz", 10, 20),
        ("new-1.0.0.tgz", 0, 40),
    ] {
        let at = now - TimeDelta::days(idle_days);
        h.store
            .put_artifact(Artifact {
                repo_id: h.repository.id,
                path: path.to_string(),
                version: "1.0.0".to_string(),
                blob_sha256: path.to_string(),
                size,
                cached_at: at,
                last_accessed_at: at,
                cached_by: "alice".to_string(),
                ..Default::default()
            })
            .await
            .expect("put artifact");
    }
    let id = h.repository.id;

    let resp = admin_do_on(
        &h.app,
        Method::GET,
        &format!("/repositories/{id}/retention-preview?days=30"),
        "",
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["count"], 1, "{body}");
    assert_eq!(body["size"], 10, "{body}");
    assert_eq!(body["items"][0]["path"], "old-1.0.0.tgz", "{body}");
    assert_eq!(body["items"][0]["uploaded_by"], "alice", "{body}");

    let body = admin_do_on(
        &h.app,
        Method::GET,
        &format!("/repositories/{id}/retention-preview?days=7"),
        "",
    )
    .await
    .json();
    assert_eq!(body["count"], 2, "{body}");
    assert_eq!(body["size"], 30, "{body}");
    assert_eq!(
        body["items"][0]["path"], "old-1.0.0.tgz",
        "least recently used first: {body}"
    );

    let body = admin_do_on(
        &h.app,
        Method::GET,
        &format!("/repositories/{id}/retention-preview?days=0"),
        "",
    )
    .await
    .json();
    assert_eq!(body["count"], 0, "zero days previews nothing: {body}");

    for days in ["", "-1", "36501", "soon"] {
        let resp = admin_do_on(
            &h.app,
            Method::GET,
            &format!("/repositories/{id}/retention-preview?days={days}"),
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "days={days}: {}",
            resp.text()
        );
    }
    let resp = admin_do_on(
        &h.app,
        Method::GET,
        "/repositories/999999/retention-preview?days=30",
        "",
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
}
