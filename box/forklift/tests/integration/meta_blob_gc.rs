use chrono::{Duration, Utc};

use forklift::meta::*;

/// Verifies the grace period: a blob that just became unreferenced is not a GC
/// candidate, and cannot be deleted even if a caller asks directly. This is
/// what keeps an asynchronous metadata rollback (s3 backend) from turning into
/// a dangling artifact reference.
#[tokio::test]
async fn blob_gc_grace_holds_recently_unreferenced() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "r".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();

    let art = s
        .put_artifact(Artifact {
            repo_id: repo.id,
            path: "a/1.0/a-1.0.tgz".into(),
            version: "1.0".into(),
            blob_sha256: "blobA".into(),
            size: 10,
            ..Artifact::default()
        })
        .await
        .expect("put");
    s.delete_artifact(repo.id, &art.path)
        .await
        .expect("delete artifact");

    // Inside the grace window the digest is unreferenced but off limits.
    let before_grace = Utc::now() - Duration::hours(1);
    let shas = s.list_unreferenced_blobs(10, before_grace).await.unwrap();
    assert!(
        shas.is_empty(),
        "unreferenced within grace = {shas:?}, want none"
    );
    let deleted = s.delete_blob_record("blobA", before_grace).await.unwrap();
    assert!(
        !deleted,
        "delete_blob_record removed a blob still inside its grace period"
    );
    let b = s.get_blob("blobA").await.expect("surviving row");
    assert_eq!(b.ref_count, 0);

    // Past the grace cutoff it becomes reclaimable.
    let after_grace = Utc::now();
    let shas = s.list_unreferenced_blobs(10, after_grace).await.unwrap();
    assert_eq!(shas, vec!["blobA".to_string()], "unreferenced past grace");
    let deleted = s.delete_blob_record("blobA", after_grace).await.unwrap();
    assert!(deleted, "delete past grace");
}

/// Verifies `unreferenced_since` is cleared when a digest is referenced again,
/// so a blob that briefly hit zero does not carry an old timestamp that would
/// let the next drop skip its grace period.
#[tokio::test]
async fn blob_gc_grace_restarts_after_rereference() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let repo = s
        .create_repository(Repository {
            name: "r".into(),
            format: FORMAT_NPM.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();

    let put = async |path: &str| {
        s.put_artifact(Artifact {
            repo_id: repo.id,
            path: path.into(),
            version: "1.0".into(),
            blob_sha256: "blobA".into(),
            size: 10,
            ..Artifact::default()
        })
        .await
        .unwrap_or_else(|e| panic!("put {path}: {e}"))
    };

    let first = put("a/1.0/a-1.0.tgz").await;
    s.delete_artifact(repo.id, &first.path)
        .await
        .expect("delete artifact");
    let stamped = unreferenced_since(&s).await;
    assert!(
        !stamped.is_empty(),
        "unreferenced_since not stamped when ref_count reached zero"
    );

    // A second artifact re-references the same content-addressed digest.
    let second = put("b/1.0/b-1.0.tgz").await;
    let cleared = unreferenced_since(&s).await;
    assert_eq!(
        cleared, "",
        "unreferenced_since after re-reference, want cleared"
    );

    // It must not be listed while referenced, whatever cutoff is passed.
    let shas = s
        .list_unreferenced_blobs(10, Utc::now() + Duration::hours(1))
        .await
        .unwrap();
    assert!(
        shas.is_empty(),
        "referenced blob listed as unreferenced: {shas:?}"
    );

    // Dropping it again re-stamps the timestamp, so the grace period restarts.
    s.delete_artifact(repo.id, &second.path)
        .await
        .expect("delete second artifact");
    let restamped = unreferenced_since(&s).await;
    assert!(
        !restamped.is_empty(),
        "unreferenced_since not re-stamped on the second drop"
    );
    assert!(
        restamped >= stamped,
        "unreferenced_since went backwards: {stamped:?} -> {restamped:?}"
    );
}

/// Verifies a blob written moments ago is never a GC candidate even if nothing
/// references it yet. That is the state a snapshot captures when it lands
/// between `blobs.put` and the artifact row that takes the reference;
/// reclaiming it would delete bytes an in-flight upload is about to point at.
#[tokio::test]
async fn blob_gc_grace_protects_fresh_blob() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    s.ensure_blob("blobFresh", 10).await.expect("ensure blob");
    // Stamp it as long unreferenced while leaving created_at at "now": only the
    // created_at guard can hold this row back.
    s.write(|c| {
        c.execute(
            "UPDATE blobs SET unreferenced_since = '2000-01-01T00:00:00Z' WHERE sha256 = 'blobFresh'",
            [],
        )
        .map(|_| ())
        .map_err(|e| Error::sqlite("stamp", e))
    })
    .await
    .unwrap();
    let cutoff = Utc::now() - Duration::minutes(1);
    let shas = s.list_unreferenced_blobs(10, cutoff).await.unwrap();
    assert!(shas.is_empty(), "fresh blob listed = {shas:?}, want none");
    let deleted = s.delete_blob_record("blobFresh", cutoff).await.unwrap();
    assert!(
        !deleted,
        "delete_blob_record removed a blob created inside the grace window"
    );
}

/// Reads `blobA`'s `unreferenced_since`, empty when NULL.
async fn unreferenced_since(s: &Store) -> String {
    s.read(|c| {
        c.query_row(
            "SELECT COALESCE(unreferenced_since, '') FROM blobs WHERE sha256 = 'blobA'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| Error::sqlite("read unreferenced_since", e))
    })
    .await
    .unwrap()
}
