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

/// The Statistics drill-down filters: each keeps exactly the artifacts its
/// panel counts, pages within the filtered set, and rejects combinations it
/// cannot honour.
#[tokio::test]
async fn list_artifacts_filters() {
    let srv = new_console_server().await;
    let id = mk_proxy_repo(&srv, "npmjs").await;

    // (path, version, max severity or "" for unscanned, licenses)
    let rows: [(&str, &str, &str, &[&str]); 4] = [
        ("vuln/-/vuln-1.0.0.tgz", "1.0.0", "high", &["MIT"]),
        ("clean/-/clean-1.0.0.tgz", "1.0.0", "none", &[]),
        (
            "licensed/-/licensed-1.0.0.tgz",
            "1.0.0",
            "",
            &["Apache-2.0"],
        ),
        ("plain/-/plain-1.0.0.tgz", "1.0.0", "", &[]),
    ];
    for (path, version, severity, licenses) in rows {
        srv.store
            .put_artifact(meta::Artifact {
                repo_id: id,
                path: path.to_string(),
                version: version.to_string(),
                blob_sha256: format!("sha-{path}"),
                size: 1,
                ..Default::default()
            })
            .await
            .expect("seed artifact");
        if !severity.is_empty() {
            let (eco, pkg) = repo::vuln_coordinate(meta::FORMAT_NPM, path);
            let counts = if severity == "none" {
                HashMap::new()
            } else {
                HashMap::from([(severity.to_string(), 1)])
            };
            srv.store
                .upsert_vuln_scan(&eco, &pkg, version, severity, &[], &counts, 0, &[], "osv")
                .await
                .expect("seed vuln scan");
        }
        if !licenses.is_empty() {
            let (system, pkg) = repo::license_coordinate(meta::FORMAT_NPM, path);
            let licenses: Vec<String> = licenses.iter().map(|l| (*l).to_string()).collect();
            srv.store
                .upsert_license_scan(&system, &pkg, version, &licenses, "deps.dev")
                .await
                .expect("seed license scan");
        }
    }
    srv.store
        .add_artifact_label(meta::ArtifactLabel {
            repo_id: id,
            path: "plain/-/plain-1.0.0.tgz".to_string(),
            label: "keep".to_string(),
            ..Default::default()
        })
        .await
        .expect("add label");

    let list = async |query: &str| {
        let resp = srv
            .admin_do(
                Method::GET,
                &format!("/repositories/{id}/artifacts?{query}"),
                "",
            )
            .await;
        (resp.status, resp.json())
    };
    let paths = |out: &serde_json::Value| -> Vec<String> {
        let mut paths: Vec<String> = out["artifacts"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|a| a["path"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
    };

    for (filter, want) in [
        (
            "scanned",
            vec!["clean/-/clean-1.0.0.tgz", "vuln/-/vuln-1.0.0.tgz"],
        ),
        ("clean", vec!["clean/-/clean-1.0.0.tgz"]),
        ("vulnerable", vec!["vuln/-/vuln-1.0.0.tgz"]),
        (
            "licensed",
            vec!["licensed/-/licensed-1.0.0.tgz", "vuln/-/vuln-1.0.0.tgz"],
        ),
        ("labeled", vec!["plain/-/plain-1.0.0.tgz"]),
        ("broken", vec![]),
    ] {
        let (status, out) = list(&format!("filter={filter}")).await;
        assert_eq!(status, StatusCode::OK, "{filter}: {out}");
        assert_eq!(paths(&out), want, "{filter}: {out}");
        assert_eq!(out["filtered"], want.len(), "{filter}: {out}");
        assert_eq!(out["count"], 4, "{filter} must not narrow count: {out}");
    }

    // Paging runs within the filtered set, and q narrows it further.
    let (_, out) = list("filter=scanned&limit=1&offset=1").await;
    assert!(
        out["filtered"] == 2 && out["artifacts"].as_array().map(Vec::len) == Some(1),
        "paged filter: {out}"
    );
    let (_, out) = list("filter=licensed&q=vuln").await;
    assert_eq!(
        paths(&out),
        vec!["vuln/-/vuln-1.0.0.tgz"],
        "filter with q: {out}"
    );
    let (_, out) = list("filter=labeled&q=vuln").await;
    assert_eq!(out["filtered"], 0, "labeled filter with q: {out}");

    for query in [
        "filter=bogus",
        "filter=clean&regex=true&q=x",
        "filter=clean&prefix=a",
    ] {
        let (status, out) = list(query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {out}");
    }
}
