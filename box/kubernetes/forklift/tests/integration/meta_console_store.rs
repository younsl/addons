use chrono::{Duration, Utc};

use forklift::meta::*;

#[tokio::test]
async fn license_scan_store() {
    let (s, _dir) = forklift::testing::meta::test_store().await;

    s.upsert_license_scan("npm", "left-pad", "1.0.0", &["MIT".to_string()], "deps.dev")
        .await
        .expect("upsert");
    let got = s
        .get_license_scan("npm", "left-pad", "1.0.0")
        .await
        .expect("get");
    assert_eq!(got.licenses, vec!["MIT".to_string()], "get = {got:?}");

    // Upsert again refreshes licenses and defaults the source.
    s.upsert_license_scan("npm", "left-pad", "1.0.0", &[], "")
        .await
        .unwrap();
    let got = s
        .get_license_scan("npm", "left-pad", "1.0.0")
        .await
        .unwrap();
    assert!(got.licenses.is_empty(), "after refresh = {got:?}");
    assert_eq!(got.source, "deps.dev", "after refresh = {got:?}");

    let err = s
        .get_license_scan("npm", "nope", "9.9.9")
        .await
        .unwrap_err();
    assert!(err.is_not_found(), "get missing err = {err}");

    let keys = s.resolved_license_keys().await.unwrap();
    assert!(
        keys.contains("npm\u{0}left-pad\u{0}1.0.0"),
        "resolved keys missing coordinate: {keys:?}"
    );

    let stale = s
        .list_stale_license_scans(Utc::now() + Duration::hours(1), 10)
        .await
        .unwrap();
    assert_eq!(stale.len(), 1, "stale, want 1");
    let none = s
        .list_stale_license_scans(Utc::now() - Duration::hours(1), 10)
        .await
        .unwrap();
    assert!(none.is_empty(), "stale before past cutoff, want 0");
}

#[tokio::test]
async fn login_tracking() {
    let (s, _dir) = forklift::testing::meta::test_store().await;
    let u = s
        .create_user(User {
            username: "bob".into(),
            password_hash: "h".into(),
            ..User::default()
        })
        .await
        .unwrap();

    s.touch_last_login(u.id).await.expect("touch");
    assert!(
        s.get_user(u.id).await.unwrap().last_login_at.is_some(),
        "last_login_at not recorded"
    );

    // Opt into lockout, then fail up to the threshold.
    s.set_lockout_enabled(u.id, true).await.unwrap();
    for _ in 0..3 {
        s.register_failed_login(u.id, 3)
            .await
            .expect("register fail");
    }
    let got = s.get_user_by_username("bob").await.unwrap();
    assert_eq!(got.failed_login_count, 3, "after 3 fails");
    assert!(got.locked(), "after 3 fails: not locked");

    // Reset clears the count and unlocks.
    s.reset_failed_login(u.id).await.unwrap();
    let got = s.get_user_by_username("bob").await.unwrap();
    assert_eq!(got.failed_login_count, 0, "after reset");
    assert!(!got.locked(), "after reset: still locked");

    // Disabling lockout also clears any accumulated failures.
    s.register_failed_login(u.id, 3).await.unwrap();
    s.set_lockout_enabled(u.id, false).await.unwrap();
    let got = s.get_user_by_username("bob").await.unwrap();
    assert_eq!(got.failed_login_count, 0, "after disable lockout");
    assert!(!got.locked(), "after disable lockout: still locked");
}

///
/// Reads must not queue behind a write. The write pool is a single connection
/// by design (single-writer SQLite), so any read that ran on it would wait for
/// whatever statement holds it. The write connection is held in an open
/// statement here and each read is then given a deadline far shorter than that
/// hold: on the write pool every one would block until it ends; on the read
/// pool they answer immediately.
#[tokio::test]
async fn reads_do_not_wait_for_the_write_connection() {
    let (store, _dir) = forklift::testing::meta::test_store().await;
    let store = std::sync::Arc::new(store);

    store
        .insert_audit_log(AuditLog {
            repo_name: "npm-hosted".into(),
            event: EVENT_DOWNLOAD.into(),
            path: "lodash/-/lodash-4.17.21.tgz".into(),
            status: 200,
            ..AuditLog::default()
        })
        .await
        .unwrap();
    store
        .create_user(User {
            username: "reader".into(),
            ..User::default()
        })
        .await
        .unwrap();

    // Occupy the write connection for longer than any deadline below.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = std::sync::Arc::clone(&store);
    let held = tokio::spawn(async move {
        holder
            .write(move |c| {
                c.execute(
                    "INSERT INTO blobs(sha256, size, ref_count, created_at) VALUES('sha-held', 1, 0, ?)",
                    rusqlite::params![now_rfc3339()],
                )
                .map_err(|e| Error::sqlite("write inside the held statement", e))?;
                let _ = released.blocking_recv();
                Ok(())
            })
            .await
            .unwrap();
    });
    // Let the holding task actually take the connection.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    with_deadline("vuln_severity_by_coordinate", async {
        store.vuln_severity_by_coordinate().await.map(|_| ())
    })
    .await;
    with_deadline("list_users", async { store.list_users().await.map(|_| ()) }).await;
    with_deadline("list_audit_logs", async {
        store
            .list_audit_logs("npm-hosted", "", 10, 0)
            .await
            .map(|_| ())
    })
    .await;

    let _ = release.send(());
    held.await.unwrap();
}

/// Fails the test when `call` does not answer well inside the window the held
/// write statement stays open for.
async fn with_deadline(name: &str, call: impl Future<Output = Result<()>>) {
    match tokio::time::timeout(std::time::Duration::from_secs(2), call).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("{name} failed: {e}"),
        Err(_) => panic!("{name} blocked behind the open write statement"),
    }
}
