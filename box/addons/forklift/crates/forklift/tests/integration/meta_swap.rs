use rusqlite::params;

use forklift::meta::*;

#[tokio::test]
async fn snapshot_and_swap() {
    let (leader, leader_dir) = forklift::testing::meta::test_store().await;
    leader
        .create_repository(Repository {
            name: "snap-repo".into(),
            format: FORMAT_GO.into(),
            r#type: TYPE_PROXY.into(),
            upstream_url: "https://proxy.golang.org".into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    let snapshot = leader_dir.path().join("snapshot.db");
    leader.snapshot(&snapshot).await.expect("snapshot");

    // Snapshot must overwrite a stale destination file.
    leader.snapshot(&snapshot).await.expect("re-snapshot");

    let (standby, _standby_dir) = forklift::testing::meta::test_store().await;
    standby
        .create_repository(Repository {
            name: "standby-only".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    standby.swap_from_snapshot(&snapshot).await.expect("swap");

    // The snapshot's data replaces the standby's local data on the same handle.
    standby
        .get_repository_by_name("snap-repo")
        .await
        .expect("snapshot repo not visible after swap");
    assert!(
        standby
            .get_repository_by_name("standby-only")
            .await
            .is_err(),
        "pre-swap local repo should be gone"
    );
    // The snapshot file is consumed by the rename.
    assert!(
        !snapshot.exists(),
        "snapshot file should be moved into place"
    );
    // Writes keep working after the swap.
    standby
        .create_repository(Repository {
            name: "post-swap".into(),
            format: FORMAT_CARGO.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .expect("write after swap");
}

#[tokio::test]
async fn list_blob_digests() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    for i in 0..5 {
        let sha = format!("{i:064}");
        s.write(move |c| {
            c.execute(
                "INSERT INTO blobs(sha256, size, ref_count, created_at) VALUES(?, 1, 1, ?)",
                params![sha, now_rfc3339()],
            )
            .map(|_| ())
            .map_err(|e| Error::sqlite("insert blob", e))
        })
        .await
        .unwrap();
    }

    let page1 = s.list_blob_digests("", 3).await.unwrap();
    assert_eq!(page1.len(), 3, "page1 = {page1:?}");
    let page2 = s.list_blob_digests(page1.last().unwrap(), 3).await.unwrap();
    assert_eq!(page2.len(), 2, "page2 = {page2:?}");
    let all: Vec<String> = page1.into_iter().chain(page2).collect();
    for i in 1..all.len() {
        assert!(all[i - 1] < all[i], "not strictly ordered: {all:?}");
    }
}
