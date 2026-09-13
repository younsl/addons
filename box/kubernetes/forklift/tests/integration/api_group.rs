use http::{Method, StatusCode};

use forklift::testing::api::new_test_server;

#[tokio::test]
async fn create_group_repository() {
    let srv = new_test_server().await;

    // Members first.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"maven-hosted","format":"maven","type":"hosted"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"maven-central","format":"maven","type":"proxy","upstream_url":"https://repo1.maven.org/maven2"}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // A valid group.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"maven-public","format":"maven","type":"group",
		  "config":{"group":{"members":["maven-hosted","maven-central"]}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let created = resp.json();
    assert_eq!(created["type"], "group", "{created}");
    assert_eq!(
        created["config"]["group"]["members"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "{created}"
    );
    let id = created["id"].as_i64().expect("repository id");

    // Invalid groups.
    for (name, body) in [
        (
            "no members",
            r#"{"name":"g1","format":"maven","type":"group"}"#,
        ),
        (
            "missing member",
            r#"{"name":"g2","format":"maven","type":"group","config":{"group":{"members":["nope"]}}}"#,
        ),
        (
            "format mismatch",
            r#"{"name":"g3","format":"npm","type":"group","config":{"group":{"members":["maven-hosted"]}}}"#,
        ),
        (
            "nested group",
            r#"{"name":"g4","format":"maven","type":"group","config":{"group":{"members":["maven-public"]}}}"#,
        ),
    ] {
        let resp = srv.admin_do(Method::POST, "/repositories", body).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{name}");
    }

    // Members on a non-group repository are rejected.
    let resp = srv
        .admin_do(
            Method::POST,
            "/repositories",
            r#"{"name":"bad-local","format":"maven","type":"hosted","config":{"group":{"members":["maven-hosted"]}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());

    // Update: reordering members is allowed, removing all is rejected.
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/repositories/{id}"),
            r#"{"upstream_url":"","config":{"group":{"members":["maven-central","maven-hosted"]}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let updated = resp.json();
    assert_eq!(
        updated["config"]["group"]["members"][0], "maven-central",
        "{updated}"
    );
    let resp = srv
        .admin_do(
            Method::PUT,
            &format!("/repositories/{id}"),
            r#"{"upstream_url":"","config":{"group":{"members":[]}}}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
}
