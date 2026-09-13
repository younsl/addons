use std::collections::HashMap;

use http::{Method, StatusCode};

use forklift::meta;
use forklift::repo;

use forklift::testing::api::{mk_proxy_repo, new_console_server, query_escape};

/// Exercises the artifact listing's vulnerability and license enrichment: an
/// artifact whose coordinate has stored scans is returned with the severity,
/// advisory ids and licenses attached.
#[tokio::test]
async fn list_artifacts_enriched() {
    let srv = new_console_server().await;
    let id = mk_proxy_repo(&srv, "npmjs").await;

    const ART_PATH: &str = "left-pad/-/left-pad-1.0.0.tgz";
    const VER: &str = "1.0.0";
    srv.store
        .put_artifact(meta::Artifact {
            repo_id: id,
            path: ART_PATH.to_string(),
            version: VER.to_string(),
            blob_sha256: "sha".to_string(),
            size: 42,
            ..Default::default()
        })
        .await
        .expect("seed artifact");

    // Seed scans under the exact coordinate the lister resolves, so the match is
    // independent of per-format path parsing.
    let (eco, pkg) = repo::vuln_coordinate(meta::FORMAT_NPM, ART_PATH);
    srv.store
        .upsert_vuln_scan(
            &eco,
            &pkg,
            VER,
            "high",
            &["CVE-2026-1".to_string()],
            &HashMap::from([("high".to_string(), 1)]),
            3,
            &[],
            "osv",
        )
        .await
        .expect("seed vuln scan");
    let (system, license_pkg) = repo::license_coordinate(meta::FORMAT_NPM, ART_PATH);
    srv.store
        .upsert_license_scan(&system, &license_pkg, VER, &["MIT".to_string()], "deps.dev")
        .await
        .expect("seed license scan");

    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{id}/artifacts"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK, "list artifacts");
    let out = resp.json();
    assert_eq!(out["count"], 1, "{out}");
    assert_eq!(out["total_size"], 42, "{out}");
    let artifacts = out["artifacts"].as_array().expect("artifacts");
    assert_eq!(artifacts.len(), 1, "{out}");
    let a = &artifacts[0];
    assert_eq!(a["max_severity"], "high", "enrichment not attached: {a}");
    assert_eq!(a["vuln_ids"].as_array().map(Vec::len), Some(1), "{a}");
    assert_eq!(a["licenses"][0], "MIT", "{a}");

    // A prefix filter that matches nothing returns an empty set, still 200.
    let resp = srv
        .admin_do(
            Method::GET,
            &format!(
                "/repositories/{id}/artifacts?prefix={}",
                query_escape("does-not-exist")
            ),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "prefix filter");
    assert_eq!(
        resp.json()["artifacts"].as_array().map(Vec::len),
        Some(0),
        "prefix filter returned rows"
    );
}

/// The new table fields expose usage without mixing failed requests or repos.
#[tokio::test]
async fn list_artifacts_download_usage() {
    let srv = new_console_server().await;
    let id = mk_proxy_repo(&srv, "usage").await;
    for path in ["used.tgz", "unused.tgz"] {
        srv.store
            .put_artifact(meta::Artifact {
                repo_id: id,
                path: path.into(),
                blob_sha256: "sha".into(),
                last_accessed_by: "alice".into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    // put_artifact preserves access ownership on updates; stamp it through the
    // download touch path used by serving as well.
    srv.store.touch(id, "used.tgz", "alice").await.unwrap();
    srv.store
        .insert_audit_logs(vec![meta::AuditLog {
            repo_name: "usage".into(),
            path: "used.tgz".into(),
            event: meta::EVENT_DOWNLOAD.into(),
            method: "GET".into(),
            status: 200,
            ..Default::default()
        }])
        .await
        .unwrap();
    let resp = srv
        .admin_do(Method::GET, &format!("/repositories/{id}/artifacts"), "")
        .await;
    assert_eq!(resp.status, StatusCode::OK);
    let out = resp.json();
    let rows = out["artifacts"].as_array().unwrap();
    let used = rows.iter().find(|a| a["path"] == "used.tgz").unwrap();
    let unused = rows.iter().find(|a| a["path"] == "unused.tgz").unwrap();
    assert_eq!(used["downloads_30d"], 1);
    assert_eq!(used["last_accessed_by"], "alice");
    assert_eq!(unused["downloads_30d"], 0);
    for query in ["q=alice", "q=ali[c]e&regex=true"] {
        let resp = srv
            .admin_do(
                Method::GET,
                &format!("/repositories/{id}/artifacts?{query}"),
                "",
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK);
        let out = resp.json();
        assert_eq!(out["artifacts"].as_array().unwrap().len(), 1);
    }
}
