use http::{Method, StatusCode};
use serde_json::Value;

use forklift::meta::{self, Artifact, ArtifactLabel};

use forklift::testing::api::mk_role_user;
use forklift::testing::api::{
    ADMIN_PASS, ADMIN_USER, TestResponse, TestServer, mk_proxy_repo, new_audit_test_server,
    query_escape,
};

fn labels_url(repo_id: i64) -> String {
    format!("/repositories/{repo_id}/artifacts/labels")
}

/// Pins who may label an artifact: an administrator on the repository, and the
/// principal recorded as having put the artifact there. A reader with no
/// ownership is refused even though it can see the artifact, and an artifact with
/// no recorded uploader has no owner to inherit the right.
#[tokio::test]
async fn artifact_label_permissions() {
    let (srv, _rec) = new_audit_test_server().await;

    let repo_id = mk_proxy_repo(&srv, "npmjs").await;
    const OWNED: &str = "lodash/-/lodash-4.17.21.tgz";
    const ANONYMOUS: &str = "left-pad/-/left-pad-1.0.0.tgz";
    for a in [
        Artifact {
            repo_id,
            path: OWNED.to_string(),
            version: "4.17.21".to_string(),
            blob_sha256: "b1".to_string(),
            size: 10,
            cached_by: "bob".to_string(),
            ..Default::default()
        },
        Artifact {
            repo_id,
            path: ANONYMOUS.to_string(),
            version: "1.0.0".to_string(),
            blob_sha256: "b2".to_string(),
            size: 5,
            ..Default::default()
        },
    ] {
        srv.store.put_artifact(a).await.expect("put artifact");
    }
    mk_role_user(
        &srv,
        "bob",
        "pw123456",
        "npm-writer",
        "npmjs",
        r#""read","write""#,
    )
    .await;
    mk_role_user(&srv, "eve", "pw123456", "npm-reader", "npmjs", r#""read""#).await;

    let add = async |user: &str, pass: &str, path: &str, label: &str| -> TestResponse {
        srv.do_as(
            user,
            pass,
            Method::POST,
            &labels_url(repo_id),
            &format!(r#"{{"path":"{path}","label":"{label}"}}"#),
        )
        .await
    };

    // The uploader labels their own artifact. Surrounding whitespace is trimmed
    // and the casing is kept as typed.
    let resp = add("bob", "pw123456", OWNED, "  Keep-Forever ").await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let list = resp.json();
    assert!(
        list["labels"].as_array().map(Vec::len) == Some(1)
            && list["labels"][0]["label"] == "Keep-Forever"
            && list["labels"][0]["created_by"] == "bob",
        "uploader add = {list}"
    );
    assert_eq!(
        list["can_label"], true,
        "uploader reported as unable to label its own artifact: {list}"
    );

    // A reader who did not upload it is refused, on both verbs.
    let resp = add("eve", "pw123456", OWNED, "eve-was-here").await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = srv
        .do_as(
            "eve",
            "pw123456",
            Method::DELETE,
            &format!(
                "{}?path={}&label=Keep-Forever",
                labels_url(repo_id),
                query_escape(OWNED)
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());

    // An artifact with no recorded uploader belongs to no one: only an admin.
    let resp = add("bob", "pw123456", ANONYMOUS, "orphan").await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = add(ADMIN_USER, ADMIN_PASS, ANONYMOUS, "orphan").await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());

    // Reading is open to anyone who can read the repository; can_label reports
    // the per-artifact answer rather than a repository-wide one.
    let resp = srv
        .do_as(
            "eve",
            "pw123456",
            Method::GET,
            &format!("{}?path={}", labels_url(repo_id), query_escape(OWNED)),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let list = resp.json();
    assert!(
        list["labels"].as_array().map(Vec::len) == Some(1) && list["can_label"] != true,
        "reader view = {list}, want the label visible and can_label false"
    );

    // Duplicates, bad values and unknown artifacts each have their own answer.
    // The charset is a key or key:value of letters, digits, '-' and '_', so a
    // space, a dotted or slashed value and a dangling separator are all refused.
    let resp = add(ADMIN_USER, ADMIN_PASS, OWNED, "Keep-Forever").await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "{}", resp.text());
    for bad in [
        "not a label",
        "sbom/verified",
        "release.1",
        "team:",
        ":payments",
        "a:b:c",
    ] {
        let resp = add(ADMIN_USER, ADMIN_PASS, OWNED, bad).await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "{bad}: {}",
            resp.text()
        );
    }
    // A key:value pair is accepted, in either case.
    let resp = add(ADMIN_USER, ADMIN_PASS, OWNED, "team:Payments").await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!(
                "{}?path={}&label=team%3APayments",
                labels_url(repo_id),
                query_escape(OWNED)
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let resp = add(ADMIN_USER, ADMIN_PASS, "no/such/artifact.tgz", "ghost").await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());

    // The admin removes the uploader's label; the listing comes back without it.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!(
                "{}?path={}&label=Keep-Forever",
                labels_url(repo_id),
                query_escape(OWNED)
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let list = resp.json();
    assert_eq!(
        list["labels"].as_array().map(Vec::len),
        Some(0),
        "after delete = {list}, want no labels"
    );
}

/// Pins the audit trail: both mutations are recorded with the label in the
/// detail, and a refused attempt is recorded too -- an operator asking "who tried
/// to touch this" must not have to infer it from a gap.
#[tokio::test]
async fn artifact_label_audit() {
    let (srv, rec) = new_audit_test_server().await;

    let repo_id = mk_proxy_repo(&srv, "npmjs").await;
    const PATH: &str = "lodash/-/lodash-4.17.21.tgz";
    srv.store
        .put_artifact(Artifact {
            repo_id,
            path: PATH.to_string(),
            version: "4.17.21".to_string(),
            blob_sha256: "b1".to_string(),
            size: 10,
            cached_by: "bob".to_string(),
            ..Default::default()
        })
        .await
        .expect("put artifact");
    mk_role_user(&srv, "eve", "pw123456", "npm-reader", "npmjs", r#""read""#).await;

    let resp = srv
        .admin_do(
            Method::POST,
            &labels_url(repo_id),
            &format!(r#"{{"path":"{PATH}","label":"keep-forever"}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let resp = srv
        .do_as(
            "eve",
            "pw123456",
            Method::POST,
            &labels_url(repo_id),
            &format!(r#"{{"path":"{PATH}","label":"eve-was-here"}}"#),
        )
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!(
                "{}?path={}&label=keep-forever",
                labels_url(repo_id),
                query_escape(PATH)
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());

    // The recorder writes asynchronously; closing it flushes the buffer.
    rec.close().await;

    let logs = srv
        .store
        .list_audit_logs("npmjs", "", 50, 0)
        .await
        .expect("list audit logs");
    let mut got: Vec<(String, String, i64, String)> = Vec::new();
    for l in &logs {
        if l.event != meta::EVENT_ARTIFACT_LABEL_ADD && l.event != meta::EVENT_ARTIFACT_LABEL_REMOVE
        {
            continue;
        }
        assert_eq!(l.path, PATH, "label event on {:?}, want {PATH:?}", l.path);
        let detail: Value = serde_json::from_str(&l.detail_json)
            .unwrap_or_else(|e| panic!("detail {:?}: {e}", l.detail_json));
        got.push((
            l.event.clone(),
            l.username.clone(),
            l.status,
            detail["label"].as_str().unwrap_or_default().to_string(),
        ));
    }
    let want: Vec<(String, String, i64, String)> = vec![
        (
            meta::EVENT_ARTIFACT_LABEL_REMOVE.to_string(),
            ADMIN_USER.to_string(),
            StatusCode::OK.as_u16() as i64,
            "keep-forever".to_string(),
        ),
        (
            meta::EVENT_ARTIFACT_LABEL_ADD.to_string(),
            "eve".to_string(),
            StatusCode::FORBIDDEN.as_u16() as i64,
            "eve-was-here".to_string(),
        ),
        (
            meta::EVENT_ARTIFACT_LABEL_ADD.to_string(),
            ADMIN_USER.to_string(),
            StatusCode::CREATED.as_u16() as i64,
            "keep-forever".to_string(),
        ),
    ];
    assert_eq!(
        got.len(),
        want.len(),
        "label audit events = {got:?}, want {want:?}"
    );
    for i in 0..want.len() {
        assert_eq!(got[i], want[i], "audit[{i}]");
    }
}

/// Pins the label fields on the surfaces that render artifacts: the repository
/// listing carries each row's labels and the caller's per-row permission, and the
/// global search offers labels as their own section.
#[tokio::test]
async fn artifact_labels_in_listings() {
    let (srv, _rec) = new_audit_test_server().await;

    let npm_id = mk_proxy_repo(&srv, "npmjs").await;
    let priv_id = mk_proxy_repo(&srv, "npm-internal").await;
    const PATH: &str = "lodash/-/lodash-4.17.21.tgz";
    for a in [
        Artifact {
            repo_id: npm_id,
            path: PATH.to_string(),
            version: "4.17.21".to_string(),
            blob_sha256: "b1".to_string(),
            size: 10,
            cached_by: "bob".to_string(),
            ..Default::default()
        },
        Artifact {
            repo_id: priv_id,
            path: "secret/-/secret-1.0.0.tgz".to_string(),
            version: "1.0.0".to_string(),
            blob_sha256: "b2".to_string(),
            size: 5,
            ..Default::default()
        },
    ] {
        srv.store.put_artifact(a).await.expect("put artifact");
    }
    srv.store
        .add_artifact_label(ArtifactLabel {
            repo_id: npm_id,
            path: PATH.to_string(),
            label: "keep-forever".to_string(),
            created_by: "bob".to_string(),
            ..Default::default()
        })
        .await
        .expect("add label");
    srv.store
        .add_artifact_label(ArtifactLabel {
            repo_id: priv_id,
            path: "secret/-/secret-1.0.0.tgz".to_string(),
            label: "keep-forever".to_string(),
            created_by: ADMIN_USER.to_string(),
            ..Default::default()
        })
        .await
        .expect("add label");
    mk_role_user(
        &srv,
        "bob",
        "pw123456",
        "npm-writer",
        "npmjs",
        r#""read","write""#,
    )
    .await;
    mk_role_user(&srv, "eve", "pw123456", "npm-reader", "npmjs", r#""read""#).await;

    let listing = async |srv: &TestServer, user: &str, pass: &str| -> Value {
        let resp = srv
            .do_as(
                user,
                pass,
                Method::GET,
                &format!("/repositories/{npm_id}/artifacts"),
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        resp.json()
    };

    let got = listing(&srv, "bob", "pw123456").await;
    assert!(
        got["artifacts"].as_array().map(Vec::len) == Some(1)
            && got["artifacts"][0]["labels"].as_array().map(Vec::len) == Some(1)
            && got["artifacts"][0]["labels"][0]["label"] == "keep-forever",
        "uploader listing = {}",
        got["artifacts"]
    );
    assert_eq!(
        got["artifacts"][0]["can_label"], true,
        "uploader row reported can_label false: {}",
        got["artifacts"][0]
    );
    let got = listing(&srv, "eve", "pw123456").await;
    assert!(
        got["artifacts"][0]["labels"].as_array().map(Vec::len) == Some(1)
            && got["artifacts"][0]["can_label"] != true,
        "reader listing = {}, want the label with can_label false",
        got["artifacts"][0]
    );

    // A search term naming a label finds the artifacts carrying it.
    let resp = srv
        .do_as(
            "eve",
            "pw123456",
            Method::GET,
            &format!("/repositories/{npm_id}/artifacts?q=keep-forever"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let page = resp.json();
    assert!(
        page["filtered"] == 1 && page["artifacts"].as_array().map(Vec::len) == Some(1),
        "repo search by label = {page}"
    );

    // Labeling coverage counts labeled artifacts, not labels, across the whole
    // repository whatever the search: a second label on the same artifact and
    // an unlabeled neighbour leave it at one of two.
    srv.store
        .add_artifact_label(ArtifactLabel {
            repo_id: npm_id,
            path: PATH.to_string(),
            label: "team:web".to_string(),
            created_by: "bob".to_string(),
            ..Default::default()
        })
        .await
        .expect("add second label");
    srv.store
        .put_artifact(Artifact {
            repo_id: npm_id,
            path: "left-pad/-/left-pad-1.3.0.tgz".to_string(),
            version: "1.3.0".to_string(),
            blob_sha256: "b3".to_string(),
            size: 3,
            ..Default::default()
        })
        .await
        .expect("put unlabeled artifact");
    let got = listing(&srv, "eve", "pw123456").await;
    assert!(
        got["count"] == 2 && got["labeled_count"] == 1,
        "labeling coverage = {} of {}",
        got["labeled_count"],
        got["count"]
    );
    let resp = srv
        .do_as(
            "eve",
            "pw123456",
            Method::GET,
            &format!("/repositories/{npm_id}/artifacts?q=left-pad"),
            "",
        )
        .await;
    assert_eq!(
        resp.json()["labeled_count"],
        1,
        "search narrowed the coverage"
    );

    // The sidebar's label section is narrowed to readable repositories, and the
    // count reflects the same narrowing rather than the global total.
    let resp = srv
        .do_as("eve", "pw123456", Method::GET, "/search?q=keep", "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let search = resp.json();
    assert!(
        search["labels"].as_array().map(Vec::len) == Some(1)
            && search["labels"][0]["repo_name"] == "npmjs"
            && search["labels"][0]["label"] == "keep-forever",
        "reader label search = {}",
        search["labels"]
    );
    assert_eq!(
        search["counts"]["labels"], 1,
        "reader label count = {}, want 1",
        search["counts"]
    );
    let resp = srv.admin_do(Method::GET, "/search?q=keep", "").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let search = resp.json();
    assert_eq!(
        search["counts"]["labels"], 2,
        "admin label count = {}, want 2",
        search["counts"]
    );
}
