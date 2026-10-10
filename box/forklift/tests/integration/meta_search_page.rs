use regex::Regex;

use forklift::meta::*;

/// Seeds one proxy repository with 120 artifacts, alternating the cached-by
/// principal so a non-path column can be searched.
async fn seed_search_artifacts(s: &Store) -> i64 {
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
    for i in 0..120 {
        s.put_artifact(Artifact {
            repo_id: repo.id,
            path: format!("seed/pkg-{i:03}/-/pkg-{i:03}-1.0.{i}.tgz"),
            version: format!("1.0.{i}"),
            blob_sha256: format!("sha-{i:03}"),
            size: 10,
            content_type: "application/octet-stream".into(),
            cached_by: if i % 2 == 0 {
                "alice".into()
            } else {
                "bob".into()
            },
            ..Artifact::default()
        })
        .await
        .unwrap();
    }
    repo.id
}

#[tokio::test]
async fn search_repo_artifacts_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo_id = seed_search_artifacts(&s).await;

    // Empty q pages through everything.
    let (page, total) = s.search_repo_artifacts(repo_id, "", 50, 0).await.unwrap();
    assert_eq!((page.len(), total), (50, 120), "empty q");
    // Offset past the end yields an empty page but the true total.
    let (page, total) = s.search_repo_artifacts(repo_id, "", 50, 200).await.unwrap();
    assert_eq!((page.len(), total), (0, 120), "offset past end");
    // Mid-keyword match on path, case-insensitive.
    let (_, total) = s
        .search_repo_artifacts(repo_id, "PKG-00", 50, 0)
        .await
        .unwrap();
    assert_eq!(total, 10, "keyword total");
    // Match on a non-path column (cached_by).
    let (_, total) = s
        .search_repo_artifacts(repo_id, "alice", 200, 0)
        .await
        .unwrap();
    assert_eq!(total, 60, "cached_by total");
    // LIKE metacharacters in q are literals, not wildcards.
    let (_, total) = s.search_repo_artifacts(repo_id, "%", 50, 0).await.unwrap();
    assert_eq!(total, 0, "literal % total");
}

#[tokio::test]
async fn search_repo_artifacts_regex_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo_id = seed_search_artifacts(&s).await;

    let re = Regex::new(r"(?i)pkg-0[0-4]\d-1\.0").unwrap();
    let (page, total) = s
        .search_repo_artifacts_regex(repo_id, &re, 20, 0)
        .await
        .unwrap();
    assert_eq!((page.len(), total), (20, 50), "regex");
    // Last page is the remainder.
    let (page, total) = s
        .search_repo_artifacts_regex(repo_id, &re, 20, 40)
        .await
        .unwrap();
    assert_eq!((page.len(), total), (10, 50), "regex last page");
}
