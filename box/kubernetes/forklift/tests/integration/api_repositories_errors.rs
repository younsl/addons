use http::{Method, StatusCode};

use forklift::testing::api::new_test_server;

/// Drives the repository-create rejection branches.
#[tokio::test]
async fn repository_create_validation() {
    let srv = new_test_server().await;
    for (name, body) in [
        ("bad json", "{"),
        (
            "invalid name",
            r#"{"name":"Bad Name!","format":"maven","type":"hosted"}"#,
        ),
        (
            "invalid format",
            r#"{"name":"r1","format":"cobol","type":"hosted"}"#,
        ),
        (
            "invalid type",
            r#"{"name":"r2","format":"maven","type":"weird"}"#,
        ),
        (
            "proxy needs upstream",
            r#"{"name":"r3","format":"maven","type":"proxy"}"#,
        ),
    ] {
        let resp = srv.admin_do(Method::POST, "/repositories", body).await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{name}");
    }
}

#[tokio::test]
async fn repository_create_duplicate() {
    let srv = new_test_server().await;
    let body = r#"{"name":"dupe","format":"maven","type":"hosted"}"#;
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(resp.status, StatusCode::CREATED, "first create");
    let resp = srv.admin_do(Method::POST, "/repositories", body).await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "duplicate create");
}

#[tokio::test]
async fn repository_missing_ids() {
    let srv = new_test_server().await;
    let resp = srv.admin_do(Method::GET, "/repositories/99999", "").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "get missing");

    let resp = srv
        .admin_do(Method::PUT, "/repositories/99999", r#"{"config":{}}"#)
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "update missing");

    let resp = srv
        .admin_do(Method::DELETE, "/repositories/99999", "")
        .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "delete missing");

    let resp = srv
        .admin_do(Method::GET, "/repositories/not-a-number", "")
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "non-numeric id");
}
