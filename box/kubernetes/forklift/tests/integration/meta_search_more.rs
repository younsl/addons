use forklift::meta::*;

#[tokio::test]
async fn search_and_aggregate_store_methods() {
    let (s, _dir) = forklift::testing::meta::test_store().await;

    let repo = s
        .create_repository(Repository {
            name: "search-repo".into(),
            format: FORMAT_MAVEN.into(),
            r#type: TYPE_HOSTED.into(),
            ..Repository::default()
        })
        .await
        .unwrap();
    s.put_artifact(Artifact {
        repo_id: repo.id,
        path: "com/acme/widget/1.0.0/widget-1.0.0.jar".into(),
        version: "1.0.0".into(),
        blob_sha256: "search-digest".into(),
        size: 8,
        ..Artifact::default()
    })
    .await
    .unwrap();

    let hits = s
        .search_artifacts("widget", 10)
        .await
        .expect("search_artifacts");
    assert!(
        !hits.is_empty(),
        "search_artifacts found nothing for 'widget'"
    );
    s.search_artifact_counts_by_repo("widget")
        .await
        .expect("search_artifact_counts_by_repo");

    // Blob + scan-target aggregates.
    s.ensure_blob("search-digest", 8)
        .await
        .expect("ensure_blob");
    s.all_scan_targets().await.expect("all_scan_targets");

    assert!(
        is_ui_managed_aggregate_metadata(r#"{"managed_by":"ui_upload_aggregate"}"#),
        "is_ui_managed_aggregate_metadata false for aggregate metadata"
    );
    assert!(
        !is_ui_managed_aggregate_metadata("{}"),
        "is_ui_managed_aggregate_metadata true for plain metadata"
    );
}
