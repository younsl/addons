use forklift::meta::*;

#[tokio::test]
async fn set_repository_disabled_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "r".into(),
            format: FORMAT_MAVEN.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    s.set_repository_disabled(repo.id, true)
        .await
        .expect("disable");
    let got = s.get_repository(repo.id).await.unwrap();
    assert!(got.disabled, "expected repository disabled");
    s.set_repository_disabled(repo.id, false).await.unwrap();
    let got = s.get_repository(repo.id).await.unwrap();
    assert!(!got.disabled, "expected repository enabled");
}

#[tokio::test]
async fn blob_stats_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "r".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_PROXY.into(),
            upstream_url: "https://registry.npmjs.org".into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    assert_eq!(s.blob_stats().await.unwrap(), (0, 0), "empty blob stats");
    for a in [
        Artifact {
            repo_id: repo.id,
            path: "a/-/a-1.tgz".into(),
            version: "1".into(),
            blob_sha256: "sha-a".into(),
            size: 10,
            ..Artifact::default()
        },
        Artifact {
            repo_id: repo.id,
            path: "b/-/b-1.tgz".into(),
            version: "1".into(),
            blob_sha256: "sha-b".into(),
            size: 25,
            ..Artifact::default()
        },
    ] {
        s.put_artifact(a).await.unwrap();
    }
    assert_eq!(s.blob_stats().await.unwrap(), (2, 35), "blob stats");
}

#[tokio::test]
async fn list_scan_targets_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "r".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_PROXY.into(),
            upstream_url: "https://registry.npmjs.org".into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    // Versioned artifacts are scan targets; a versionless one is excluded.
    for (path, version, sha) in [
        ("a/-/a-1.tgz", "1.0.0", "sa"),
        ("b/-/b-2.tgz", "2.0.0", "sb"),
        ("meta.json", "", "sc"),
    ] {
        s.put_artifact(Artifact {
            repo_id: repo.id,
            path: path.into(),
            version: version.into(),
            blob_sha256: sha.into(),
            size: 1,
            ..Artifact::default()
        })
        .await
        .unwrap();
    }
    let targets = s.list_scan_targets(100, 0).await.expect("scan targets");
    assert_eq!(targets.len(), 2);
    assert_eq!(
        targets[0].format, FORMAT_NPM,
        "unexpected target: {:?}",
        targets[0]
    );
    assert!(
        !targets[0].version.is_empty(),
        "unexpected target: {:?}",
        targets[0]
    );
}
