use chrono::{Duration, TimeZone, Utc};

use forklift::meta::*;

#[tokio::test]
async fn blob_record_lifecycle() {
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
    let art = s
        .put_artifact(Artifact {
            repo_id: repo.id,
            path: "a/b.jar".into(),
            blob_sha256: "deadbeef".into(),
            size: 4,
            ..Artifact::default()
        })
        .await
        .unwrap();

    // Referenced blobs are not deletable candidates.
    let shas = s.list_unreferenced_blobs(10, Utc::now()).await.unwrap();
    assert!(shas.is_empty(), "unreferenced = {shas:?}, want none");

    // Deleting the repo unreferences the blob; the record can then be removed.
    s.delete_repository(repo.id).await.unwrap();
    let shas = s.list_unreferenced_blobs(10, Utc::now()).await.unwrap();
    assert_eq!(shas, vec![art.blob_sha256.clone()], "unreferenced");
    let deleted = s
        .delete_blob_record(&art.blob_sha256, Utc::now())
        .await
        .unwrap();
    assert!(
        deleted,
        "delete of unreferenced blob reported deleted = false"
    );
    let err = s.get_blob(&art.blob_sha256).await.unwrap_err();
    assert!(
        err.is_not_found(),
        "get after delete err = {err}, want NotFound"
    );
    // Deleting an already-removed record is a sweeper-friendly no-op.
    let deleted = s
        .delete_blob_record(&art.blob_sha256, Utc::now())
        .await
        .expect("double delete");
    assert!(!deleted, "double delete reported deleted = true");
}

#[tokio::test]
async fn purge_artifacts() {
    let (s, _dir) = forklift::testing::meta::test_store().await;

    let repo = s
        .create_repository(Repository {
            name: "purge-me".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    let other = s
        .create_repository(Repository {
            name: "keep-me".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();

    // Two artifacts in the target repo share one blob (ref_count must drop by
    // 2), a third has its own. A fourth lives in another repo and must survive.
    for (repo_id, path, sha, size) in [
        (repo.id, "a/1.tgz", "shared", 10),
        (repo.id, "a/2.tgz", "shared", 10),
        (repo.id, "a/3.tgz", "solo", 5),
        (other.id, "b/1.tgz", "elsewhere", 3),
    ] {
        s.put_artifact(Artifact {
            repo_id,
            path: path.into(),
            blob_sha256: sha.into(),
            size,
            ..Artifact::default()
        })
        .await
        .unwrap();
    }

    let n = s.purge_artifacts(repo.id).await.unwrap();
    assert_eq!(n, 3, "purged");
    assert_eq!(
        s.count_artifacts(repo.id).await.unwrap(),
        0,
        "repo still has artifacts"
    );
    // The other repo is untouched.
    assert_eq!(
        s.count_artifacts(other.id).await.unwrap(),
        1,
        "other repo count"
    );
    // All blobs the purged repo referenced are now unreferenced; the shared blob
    // must not be pinned by a leaked reference.
    let shas = s.list_unreferenced_blobs(10, Utc::now()).await.unwrap();
    assert_eq!(
        shas.len(),
        2,
        "unreferenced blobs = {shas:?}, want shared+solo"
    );

    // Purging an already-empty repo is a no-op returning 0.
    assert_eq!(s.purge_artifacts(repo.id).await.unwrap(), 0, "re-purge");
}

#[tokio::test]
async fn list_expired_artifacts() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "idle".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();

    let base = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    for (path, last_accessed) in [
        ("old-3h", base - Duration::hours(3)),
        ("old-2h", base - Duration::hours(2)),
        ("fresh", base),
    ] {
        s.put_artifact(Artifact {
            repo_id: repo.id,
            path: path.into(),
            blob_sha256: path.into(),
            size: 1,
            cached_at: last_accessed,
            last_accessed_at: last_accessed,
            ..Artifact::default()
        })
        .await
        .unwrap();
    }

    // Cutoff 1h before base: both old artifacts qualify, the fresh one does not.
    let got = s
        .list_expired_artifacts(repo.id, base - Duration::hours(1), 10)
        .await
        .unwrap();
    assert_eq!(got.len(), 2, "expired");
    // Oldest first.
    assert_eq!(
        (got[0].path.as_str(), got[1].path.as_str()),
        ("old-3h", "old-2h"),
        "order"
    );

    // Limit is honored.
    let got = s
        .list_expired_artifacts(repo.id, base - Duration::hours(1), 1)
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, "old-3h", "limited");

    // A cutoff before everything matches nothing.
    let got = s
        .list_expired_artifacts(repo.id, base - Duration::hours(100), 10)
        .await
        .unwrap();
    assert!(got.is_empty(), "expired before all");
}

#[tokio::test]
async fn store_accessors() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    s.ping().await.expect("ping");
    assert!(!s.path().as_os_str().is_empty(), "path empty");
    let (write, read) = s.pool_stats();
    assert_eq!(write.max_open, 1);
    assert_eq!(read.max_open, READ_POOL_SIZE as u64);
}

#[tokio::test]
async fn all_repo_stats() {
    let (s, _dir) = forklift::testing::meta::test_store().await;

    let r1 = s
        .create_repository(Repository {
            name: "stats-a".into(),
            format: FORMAT_MAVEN.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    let r2 = s
        .create_repository(Repository {
            name: "stats-b".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    for (i, (repo_id, path, sha, size)) in [
        (r1.id, "a/1.jar", "b1", 100),
        (r1.id, "a/2.jar", "b2", 50),
        (r2.id, "p/-/p-1.tgz", "b3", 7),
    ]
    .into_iter()
    .enumerate()
    {
        s.put_artifact(Artifact {
            repo_id,
            path: path.into(),
            blob_sha256: sha.into(),
            size,
            ..Artifact::default()
        })
        .await
        .unwrap_or_else(|e| panic!("put {i}: {e}"));
    }

    let stats = s.all_repo_stats().await.unwrap();
    assert_eq!(
        stats[&r1.id],
        RepoStats {
            artifact_count: 2,
            total_size: 150
        },
        "r1 stats"
    );
    assert_eq!(
        stats[&r2.id],
        RepoStats {
            artifact_count: 1,
            total_size: 7
        },
        "r2 stats"
    );
    assert!(
        !stats.contains_key(&999),
        "unexpected stats for unknown repo"
    );
}
