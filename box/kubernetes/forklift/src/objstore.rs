//! Syncs the SQLite metadata database to S3 for the object-storage HA mode.
//! SQLite cannot run live on S3 (it needs POSIX file locking), so the live
//! database stays on a local volume (typically an emptyDir) and this module
//! keeps a durable copy in S3: the leader periodically uploads a `VACUUM INTO`
//! snapshot, and every pod restores the latest snapshot on boot and applies it
//! on promotion.
//!
//! It reuses [`meta::Store::snapshot`] / [`meta::Store::swap_from_snapshot`]
//! and mirrors the leader/standby control flow of the PV-based replicator. The
//! tradeoff is the same: replication is asynchronous, so a crash can lose the
//! writes made since the last cycle (an orderly demotion or shutdown flushes a
//! final snapshot).
//!
//! The invariant that keeps this safe is that S3 holds the only authoritative
//! copy: a leader always promotes onto the object that is current at that
//! moment, never onto its own local database. The database may therefore fall
//! behind, but it never moves backwards from what S3 already published -- which
//! is what stops a resurrected artifact row from outliving blob bytes the
//! sweeper reclaimed.

mod condprobe;
mod metasync;

pub use condprobe::{ConditionalWrites, probe_conditional_writes};
pub use metasync::{
    Error, GetObjectOutput, HeadObjectOutput, MetaOptions, MetaSync, ObjectApi, PutBody,
    PutObjectInput, PutObjectOutput, Result, S3Api,
};

/// The bucket key of the metadata snapshot under `prefix`.
pub fn meta_key(prefix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        "meta/forklift.db".to_string()
    } else {
        format!("{prefix}/meta/forklift.db")
    }
}

/// Retries `f` with exponential backoff while it fails with a transient
/// error, for at most `budget`. Any other error is returned at once, so wrong
/// credentials still fail fast.
pub async fn retry_transient<T, F, Fut>(
    what: &'static str,
    budget: std::time::Duration,
    first_delay: std::time::Duration,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    let mut delay = first_delay;
    loop {
        match f().await {
            Err(e) if e.is_transient() && tokio::time::Instant::now() + delay < deadline => {
                tracing::warn!(what, err = %e, retry_in = ?delay, "object store not ready; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(30));
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::objstore::*;

    #[tokio::test(start_paused = true)]
    async fn retries_transient_until_success() {
        let calls = AtomicUsize::new(0);
        let got = retry_transient(
            "t",
            Duration::from_secs(240),
            Duration::from_secs(1),
            || async {
                if calls.fetch_add(1, Ordering::SeqCst) < 3 {
                    Err(Error::Unavailable("connection refused".into()))
                } else {
                    Ok(7)
                }
            },
        )
        .await
        .unwrap();
        assert_eq!((got, calls.load(Ordering::SeqCst)), (7, 4));
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_errors_fail_fast() {
        let calls = AtomicUsize::new(0);
        let err = retry_transient(
            "t",
            Duration::from_secs(240),
            Duration::from_secs(1),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::ObjectStore("403 InvalidAccessKeyId".into()).context("probe"))
            },
        )
        .await
        .unwrap_err();
        assert!(!err.is_transient());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_within_budget() {
        let start = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        let err = retry_transient(
            "t",
            Duration::from_secs(240),
            Duration::from_secs(1),
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::Unavailable("down".into()).context("restore"))
            },
        )
        .await
        .unwrap_err();
        assert!(err.is_transient());
        assert!(
            start.elapsed() < Duration::from_secs(240),
            "{:?}",
            start.elapsed()
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 6,
            "backoff 1,2,4,8,16,30..."
        );
    }

    async fn api(endpoint: &str) -> S3Api {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = crate::storage::new_s3_client(&crate::storage::S3Config {
            bucket: "b".into(),
            region: "us-east-1".into(),
            endpoint: endpoint.into(),
            force_path_style: true,
            access_key_id: "a".into(),
            secret_access_key: "s".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        S3Api::new(client)
    }

    async fn head_status(status: u16, code: &str) -> Error {
        let server = wiremock::MockServer::start().await;
        let body = format!("<Error><Code>{code}</Code><Message>m</Message></Error>");
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        api(&server.uri())
            .await
            .get_object("b", "k")
            .await
            .map(|_| ())
            .unwrap_err()
    }

    #[tokio::test]
    async fn classifies_sdk_errors() {
        let unreachable = api("http://127.0.0.1:1")
            .await
            .head_object("b", "k")
            .await
            .unwrap_err();
        assert!(unreachable.is_transient(), "{unreachable}");
        assert!(
            unreachable.to_string().contains("head object"),
            "{unreachable}"
        );

        assert!(head_status(503, "ServiceUnavailable").await.is_transient());
        assert!(head_status(404, "NoSuchBucket").await.is_transient());
        let denied = head_status(403, "InvalidAccessKeyId").await;
        assert!(!denied.is_transient(), "{denied}");
        assert!(
            denied.to_string().contains("InvalidAccessKeyId"),
            "{denied}"
        );
    }

    #[tokio::test]
    async fn ensure_bucket_creates_once_and_tolerates_races() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path_regex("^/missing/?$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex("^/missing/?$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path_regex("^/present/?$"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path_regex("^/raced/?$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex("^/raced/?$"))
            .respond_with(ResponseTemplate::new(409).set_body_string(
                "<Error><Code>BucketAlreadyOwnedByYou</Code><Message>m</Message></Error>",
            ))
            .mount(&server)
            .await;
        let s3 = api(&server.uri()).await;
        assert!(s3.ensure_bucket("missing").await.unwrap(), "created");
        assert!(!s3.ensure_bucket("present").await.unwrap(), "already there");
        assert!(!s3.ensure_bucket("raced").await.unwrap(), "lost the race");

        let down = api("http://127.0.0.1:1")
            .await
            .ensure_bucket("b")
            .await
            .unwrap_err();
        assert!(down.is_transient(), "{down}");
    }

    #[test]
    fn meta_key_cases() {
        assert_eq!(meta_key(""), "meta/forklift.db");
        assert_eq!(meta_key("/p/"), "p/meta/forklift.db");
    }
}
