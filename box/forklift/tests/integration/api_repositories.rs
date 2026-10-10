use http::{Method, StatusCode};

use forklift::testing::api::*;

#[tokio::test]
async fn create_and_list_repository() {
    let srv = new_test_server().await;

    let body = r#"{"name":"npm-proxy","format":"npm","type":"proxy","upstream_url":"https://registry.npmjs.org","config":{"age_policy":{"enabled":true,"min_age":"3d","action":"block"}}}"#;
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(
        resp.status,
        StatusCode::CREATED,
        "create status: {}",
        resp.text()
    );
    let created = resp.json();
    assert_ne!(created["id"].as_i64(), Some(0), "{created}");
    assert_eq!(
        created["config"]["age_policy"]["enabled"], true,
        "{created}"
    );

    let resp = srv.admin_do(Method::GET, "/repositories", "").await;
    let list = resp.json();
    let list = list.as_array().expect("list");
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["name"], "npm-proxy", "{list:?}");
}

#[tokio::test]
async fn repository_requires_admin() {
    let srv = new_test_server().await;
    // No credentials -> 401.
    let resp = srv.anon_do(Method::GET, "/repositories", "").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "unauthenticated");
}

#[tokio::test]
async fn create_validation() {
    let srv = new_test_server().await;
    for case in [
        r#"{"name":"BAD NAME","format":"npm","type":"hosted"}"#,
        r#"{"name":"x","format":"rubygems","type":"hosted"}"#,
        r#"{"name":"x","format":"npm","type":"weird"}"#,
        // Missing upstream_url.
        r#"{"name":"x","format":"npm","type":"proxy"}"#,
    ] {
        let resp = srv.admin_do(Method::POST, "/repositories", case).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "case {case}");
    }
}

#[tokio::test]
async fn create_duplicate_conflict() {
    let srv = new_test_server().await;
    let body = r#"{"name":"dup","format":"go","type":"hosted"}"#;
    srv.admin_do(Method::POST, "/repositories", body).await;
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "dup: {}", resp.text());
}

#[tokio::test]
async fn update_and_delete() {
    let srv = new_test_server().await;
    let body = r#"{"name":"cargo-proxy","format":"cargo","type":"proxy","upstream_url":"https://index.crates.io"}"#;
    let created = srv
        .admin_do(Method::POST, "/repositories", body)
        .await
        .json();
    let id = created["id"].as_i64().expect("repository id");

    let update = r#"{"upstream_url":"https://mirror.example.com","config":{"cache":{"enabled":true,"max_size_bytes":2048,"eviction":"lru"}}}"#;
    let updated = srv
        .admin_do(Method::PUT, &format!("/repositories/{id}"), update)
        .await
        .json();
    assert_eq!(
        updated["upstream_url"], "https://mirror.example.com",
        "{updated}"
    );
    assert_eq!(
        updated["config"]["cache"]["max_size_bytes"], 2048,
        "{updated}"
    );

    let resp = srv
        .admin_do(Method::DELETE, &format!("/repositories/{id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "delete");

    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "get after delete");
}

#[tokio::test]
async fn delete_artifact() {
    let srv = new_test_server().await;
    let id = mk_proxy_repo(&srv, "npmjs").await;

    let seed = async || {
        for (path, version, sha, size) in [
            ("lodash/-/lodash-4.17.21.tgz", "4.17.21", "b1", 10),
            ("is-odd/-/is-odd-1.0.0.tgz", "1.0.0", "b2", 5),
        ] {
            srv.store
                .put_artifact(forklift::meta::Artifact {
                    repo_id: id,
                    path: path.to_string(),
                    version: version.to_string(),
                    blob_sha256: sha.to_string(),
                    size,
                    ..Default::default()
                })
                .await
                .expect("seed artifact");
        }
    };
    seed().await;

    // A single delete by path -> 204, only that artifact gone.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!(
                "/repositories/{id}/artifacts?path={}",
                query_escape("lodash/-/lodash-4.17.21.tgz")
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "single delete");
    assert_eq!(
        srv.store.count_artifacts(id).await.expect("count"),
        1,
        "count after single delete"
    );

    // Deleting a missing path -> 404.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!(
                "/repositories/{id}/artifacts?path={}",
                query_escape("nope/-/nope-9.9.9.tgz")
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "missing delete");

    // Purge all (no path) -> 200 with the deleted count, repo emptied.
    seed().await; // restores the one that was deleted; the survivor stays
    let resp = srv
        .admin_do(Method::DELETE, &format!("/repositories/{id}/artifacts"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "purge");
    assert_eq!(resp.json()["deleted"], 2, "purge deleted");
    assert_eq!(
        srv.store.count_artifacts(id).await.expect("count"),
        0,
        "count after purge"
    );

    // An unknown repo id -> 404.
    let resp = srv
        .admin_do(Method::DELETE, "/repositories/99999/artifacts", "")
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "unknown repo");
}
