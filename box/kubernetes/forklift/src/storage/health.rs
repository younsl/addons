//! Periodic reachability checks of the object store, kept as a short history
//! for the Storage admin page.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

pub const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// One hour at [`CHECK_INTERVAL`].
pub const HISTORY_LEN: usize = 60;
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds the history to a fixed size; SDK error chains can run long.
const MAX_ERROR_LEN: usize = 512;

/// One check whose `Err` carries the message shown to operators.
#[async_trait]
pub trait Probe: Send + Sync {
    async fn check(&self) -> Result<(), String>;
}

/// `HeadBucket`: the cheapest request that proves both reachability and that
/// the configured credentials can see the bucket.
pub struct HeadBucketProbe {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl HeadBucketProbe {
    pub fn new(client: aws_sdk_s3::Client, bucket: &str) -> HeadBucketProbe {
        HeadBucketProbe {
            client,
            bucket: bucket.to_string(),
        }
    }
}

#[async_trait]
impl Probe for HeadBucketProbe {
    async fn check(&self) -> Result<(), String> {
        self.client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "head bucket: {}",
                    aws_sdk_s3::error::DisplayErrorContext(&e)
                )
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthCheck {
    pub at: DateTime<Utc>,
    pub ok: bool,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Runs on every pod, not only the leader: reachability is a per-pod network
/// property, so the history describes the pod that served the request.
pub struct HealthMonitor {
    probe: Arc<dyn Probe>,
    history: Mutex<VecDeque<HealthCheck>>,
}

impl HealthMonitor {
    pub fn new(probe: Arc<dyn Probe>) -> Arc<HealthMonitor> {
        Arc::new(HealthMonitor {
            probe,
            history: Mutex::new(VecDeque::with_capacity(HISTORY_LEN)),
        })
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => self.check_once().await,
            }
        }
    }

    pub async fn check_once(&self) {
        let at = Utc::now();
        let started = Instant::now();
        let result = match tokio::time::timeout(CHECK_TIMEOUT, self.probe.check()).await {
            Ok(r) => r,
            Err(_) => Err(format!("timed out after {}s", CHECK_TIMEOUT.as_secs())),
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        if let Err(e) = &result {
            tracing::warn!(err = %e, "storage health check failed");
        }
        self.record(HealthCheck {
            at,
            ok: result.is_ok(),
            latency_ms,
            error: result.err().map(truncate).unwrap_or_default(),
        });
    }

    fn record(&self, check: HealthCheck) {
        let mut history = self.history.lock();
        if history.len() == HISTORY_LEN {
            history.pop_front();
        }
        history.push_back(check);
    }

    /// Oldest first.
    pub fn history(&self) -> Vec<HealthCheck> {
        self.history.lock().iter().cloned().collect()
    }

    pub fn last(&self) -> Option<HealthCheck> {
        self.history.lock().back().cloned()
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_ERROR_LEN {
        let mut cut = MAX_ERROR_LEN - 3;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("...");
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::storage::health::*;

    struct Scripted(AtomicUsize);

    #[async_trait]
    impl Probe for Scripted {
        async fn check(&self) -> Result<(), String> {
            if self.0.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                Ok(())
            } else {
                Err("boom".into())
            }
        }
    }

    struct Hangs;

    #[async_trait]
    impl Probe for Hangs {
        async fn check(&self) -> Result<(), String> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn records_success_and_failure() {
        let m = HealthMonitor::new(Arc::new(Scripted(AtomicUsize::new(0))));
        m.check_once().await;
        m.check_once().await;
        let h = m.history();
        assert_eq!(h.len(), 2);
        assert!(h[0].ok && h[0].error.is_empty());
        assert!(!h[1].ok);
        assert_eq!(h[1].error, "boom");
        assert!(h[0].at <= h[1].at);
    }

    #[tokio::test]
    async fn history_is_bounded() {
        let m = HealthMonitor::new(Arc::new(Scripted(AtomicUsize::new(0))));
        for _ in 0..HISTORY_LEN + 5 {
            m.check_once().await;
        }
        let h = m.history();
        assert_eq!(h.len(), HISTORY_LEN);
        assert!(h.windows(2).all(|w| w[0].at <= w[1].at));
    }

    #[test]
    fn long_errors_are_truncated_on_a_char_boundary() {
        assert_eq!(truncate("short".into()), "short");
        let out = truncate("é".repeat(MAX_ERROR_LEN));
        assert!(out.len() <= MAX_ERROR_LEN, "{}", out.len());
        assert!(out.ends_with("..."));
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_probe_times_out() {
        let m = HealthMonitor::new(Arc::new(Hangs));
        m.check_once().await;
        let h = m.history();
        assert!(!h[0].ok);
        assert!(h[0].error.contains("timed out"), "{}", h[0].error);
    }

    #[tokio::test(start_paused = true)]
    async fn run_checks_immediately_and_every_interval() {
        let m = HealthMonitor::new(Arc::new(Scripted(AtomicUsize::new(0))));
        let cancel = CancellationToken::new();
        let task = tokio::spawn(Arc::clone(&m).run(cancel.clone()));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(m.history().len(), 1);
        tokio::time::sleep(CHECK_INTERVAL).await;
        assert_eq!(m.history().len(), 2);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn head_bucket_probe_reports_errors() {
        crate::storage::admin::tests::install_crypto_provider();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("HEAD"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("HEAD"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let client = crate::storage::new_s3_client(&crate::storage::S3Config {
            bucket: "bucket".into(),
            region: "us-east-1".into(),
            endpoint: server.uri(),
            force_path_style: true,
            access_key_id: "a".into(),
            secret_access_key: "s".into(),
            ..Default::default()
        })
        .await
        .unwrap();
        let probe = HeadBucketProbe::new(client, "bucket");
        assert_eq!(probe.check().await, Ok(()));
        let err = probe.check().await.unwrap_err();
        assert!(err.starts_with("head bucket: "), "{err}");
    }
}
