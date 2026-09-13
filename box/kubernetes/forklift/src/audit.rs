//! Records per-repository audit events (artifact traffic and repository
//! configuration changes) into the metadata store. Writes are buffered through
//! a channel and flushed by a background worker so the hot request path never
//! blocks on SQLite's single write connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use prometheus::{IntCounter, Opts, Registry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::meta::{self, AuditLog};

const BUFFER_SIZE: usize = 1024;

/// Caps how many buffered events one transaction writes. The worker takes
/// whatever has queued up to this many, so under a burst the write connection
/// is acquired once per batch rather than once per event.
const BATCH_SIZE: usize = 256;

const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// One auditable occurrence on a repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    pub repo: String,
    /// A `meta::EVENT_*` constant.
    pub action: String,
    pub path: String,
    pub username: String,
    pub method: String,
    pub status: i64,
    pub client_ip: String,
    pub user_agent: String,
    pub request_id: String,
    pub detail_json: String,
}

/// Injectable clock, so tests can pin `created_at`.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Asynchronously persists audit events.
///
pub struct Recorder {
    store: Arc<meta::Store>,
    /// The send side of the buffer. Taken (dropped) by [`Recorder::close`],
    /// which is what makes the worker drain and exit.
    tx: parking_lot::Mutex<Option<mpsc::Sender<AuditLog>>>,
    /// Completion of the worker task; awaited by [`Recorder::close`].
    done: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    now: Clock,

    dropped: IntCounter,
}

impl Recorder {
    /// Builds a Recorder and starts its background writer.
    ///
    pub fn new(store: Arc<meta::Store>, registry: &Registry) -> Arc<Recorder> {
        Self::with_clock(store, registry, Arc::new(Utc::now))
    }

    /// [`Recorder::new`] with an injected clock.
    pub fn with_clock(store: Arc<meta::Store>, registry: &Registry, now: Clock) -> Arc<Recorder> {
        let dropped = IntCounter::with_opts(
            Opts::new(
                "audit_events_dropped_total",
                "Audit events dropped because the write buffer was full.",
            )
            .namespace("forklift"),
        )
        .expect("valid audit metric options");
        registry
            .register(Box::new(dropped.clone()))
            .expect("register forklift_audit_events_dropped_total");

        let (tx, rx) = mpsc::channel(BUFFER_SIZE);
        let worker_store = Arc::clone(&store);
        let handle = tokio::spawn(Self::run(worker_store, rx));
        Arc::new(Recorder {
            store,
            tx: parking_lot::Mutex::new(Some(tx)),
            done: tokio::sync::Mutex::new(Some(handle)),
            now,
            dropped,
        })
    }

    /// Enqueues an event without blocking; when the buffer is full the event is
    /// dropped and counted rather than stalling artifact traffic.
    pub fn record(&self, e: Event) {
        let l = AuditLog {
            id: 0,
            repo_name: e.repo.clone(),
            event: e.action.clone(),
            path: e.path,
            username: e.username,
            method: e.method,
            status: e.status,
            client_ip: e.client_ip,
            user_agent: e.user_agent,
            request_id: e.request_id,
            detail_json: e.detail_json,
            created_at: (self.now)(),
        };
        let tx = self.tx.lock();
        let Some(tx) = tx.as_ref() else {
            return;
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(l) {
            self.dropped.inc();
            tracing::warn!(repo = %e.repo, event = %e.action, "audit event dropped, buffer full");
        }
    }

    async fn run(store: Arc<meta::Store>, mut rx: mpsc::Receiver<AuditLog>) {
        let mut batch: Vec<AuditLog> = Vec::with_capacity(BATCH_SIZE);
        while let Some(l) = rx.recv().await {
            batch.clear();
            batch.push(l);
            // Take everything already queued (up to BATCH_SIZE) without waiting
            // for more: a quiet period still flushes each event promptly, a
            // burst is coalesced.
            while batch.len() < BATCH_SIZE {
                match rx.try_recv() {
                    Ok(more) => batch.push(more),
                    Err(_) => break,
                }
            }
            Self::flush(&store, std::mem::take(&mut batch)).await;
        }
    }

    async fn flush(store: &meta::Store, batch: Vec<AuditLog>) {
        let events = batch.len();
        let first_repo = batch[0].repo_name.clone();
        let first_event = batch[0].event.clone();
        let res = tokio::time::timeout(FLUSH_TIMEOUT, store.insert_audit_logs(batch)).await;
        let err = match res {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "context deadline exceeded".to_owned(),
        };
        tracing::error!(
            events,
            first_repo = %first_repo,
            first_event = %first_event,
            err = %err,
            "audit insert failed"
        );
    }

    /// Stops accepting events and waits for buffered ones to be written.
    pub async fn close(&self) {
        drop(self.tx.lock().take());
        let handle = self.done.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Periodically prunes audit log entries older than `retention`. It must be
    /// leader-gated by the caller in HA mode (single SQLite writer).
    pub async fn run_retention(
        self: Arc<Self>,
        cancel: CancellationToken,
        interval: Duration,
        retention: Duration,
    ) {
        if retention.is_zero() {
            return;
        }
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            self.prune_once(retention).await;
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
        }
    }

    pub(crate) async fn prune_once(&self, retention: Duration) {
        let before =
            (self.now)() - chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::MAX);
        match self.store.prune_audit_logs(before).await {
            Err(e) => tracing::error!(err = %e, "audit retention prune failed"),
            Ok(n) if n > 0 => tracing::info!(count = n, "audit retention pruned entries"),
            Ok(_) => {}
        }
    }
}

/// Extracts the originating client IP, preferring the first X-Forwarded-For hop (set by the
/// ingress) over the TCP peer address. The peer address is read from the `ConnectInfo`
/// extension the server injects per connection.
pub fn client_ip_parts(parts: &http::request::Parts) -> String {
    let remote = parts
        .extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    client_ip(&parts.headers, remote)
}

pub fn client_ip(headers: &http::HeaderMap, remote: Option<SocketAddr>) -> String {
    let remote = remote.map(|a| a.to_string()).unwrap_or_default();
    client_ip_str(headers, &remote)
}

pub fn client_ip_str(headers: &http::HeaderMap, remote_addr: &str) -> String {
    if let Some(xff) = headers
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
    {
        if let Some((first, _)) = xff.split_once(',') {
            return first.trim().to_owned();
        }
        return xff.trim().to_owned();
    }
    match split_host_port(remote_addr) {
        Some(host) => host.to_owned(),
        None => remote_addr.to_owned(),
    }
}

fn split_host_port(s: &str) -> Option<&str> {
    let (host, _port) = s.rsplit_once(':')?;
    if let Some(inner) = host.strip_prefix('[') {
        return inner.strip_suffix(']');
    }
    if host.contains(':') || host.contains('[') || host.contains(']') {
        return None;
    }
    Some(host)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::Utc;
    use prometheus::Registry;
    use tokio_util::sync::CancellationToken;

    use crate::audit::*;
    use crate::meta::{self, AuditLog};

    async fn new_recorder() -> (Arc<Recorder>, Arc<meta::Store>, tempfile::TempDir) {
        let (store, dir) = crate::testing::meta::test_store().await;
        let store = Arc::new(store);
        let rec = Recorder::new(Arc::clone(&store), &Registry::new());
        (rec, store, dir)
    }

    #[tokio::test]
    async fn recorder_flushes_on_close() {
        let (rec, store, _dir) = new_recorder().await;

        rec.record(Event {
            repo: "maven-central".into(),
            action: meta::EVENT_DOWNLOAD.into(),
            path: "a.jar".into(),
            username: "alice".into(),
            method: "GET".into(),
            status: 200,
            client_ip: "10.0.0.1".into(),
            user_agent: "maven".into(),
            ..Default::default()
        });
        rec.record(Event {
            repo: "maven-central".into(),
            action: meta::EVENT_UPLOAD.into(),
            path: "b.jar".into(),
            status: 201,
            ..Default::default()
        });
        rec.close().await;

        let logs = store
            .list_audit_logs("maven-central", "", 10, 0)
            .await
            .expect("list");
        assert_eq!(logs.len(), 2, "len = {}, want 2", logs.len());
        assert!(
            logs[1].username == "alice" && logs[1].client_ip == "10.0.0.1",
            "oldest = {:?}",
            logs[1]
        );
    }

    #[tokio::test]
    async fn nil_recorder_is_noop() {
        let rec: Option<Arc<Recorder>> = None;
        if let Some(r) = &rec {
            r.record(Event {
                repo: "r".into(),
                action: meta::EVENT_DOWNLOAD.into(),
                ..Default::default()
            });
            r.close().await;
            Arc::clone(r)
                .run_retention(
                    CancellationToken::new(),
                    Duration::from_secs(3600),
                    Duration::from_secs(3600),
                )
                .await;
        }
        assert!(rec.is_none());
    }

    #[tokio::test]
    async fn prune_once() {
        let (rec, store, _dir) = new_recorder().await;

        let old = Utc::now() - chrono::Duration::hours(48);
        store
            .insert_audit_log(AuditLog {
                repo_name: "r".into(),
                event: meta::EVENT_DOWNLOAD.into(),
                created_at: old,
                ..Default::default()
            })
            .await
            .expect("insert old");
        rec.record(Event {
            repo: "r".into(),
            action: meta::EVENT_DOWNLOAD.into(),
            ..Default::default()
        });
        rec.close().await;

        rec.prune_once(Duration::from_secs(24 * 3600)).await;
        let n = store.count_audit_logs("r", "").await.expect("count");
        assert_eq!(n, 1, "remaining = {n}, want 1");
    }

    #[tokio::test]
    async fn run_retention_stops_on_cancel() {
        let (rec, _store, _dir) = new_recorder().await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(Arc::clone(&rec).run_retention(
            cancel.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(24 * 3600),
        ));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("RunRetention did not stop on cancel")
            .expect("task join");
        rec.close().await;
    }

    #[test]
    fn client_ip_cases() {
        let mut headers = http::HeaderMap::new();
        let remote: Option<SocketAddr> = Some("192.0.2.10:54321".parse().unwrap());
        assert_eq!(client_ip(&headers, remote), "192.0.2.10", "remote addr ip");
        assert_eq!(
            client_ip_str(&headers, "192.0.2.10:54321"),
            "192.0.2.10",
            "remote addr ip (raw)"
        );

        headers.insert(
            "X-Forwarded-For",
            "203.0.113.5, 192.0.2.10".parse().unwrap(),
        );
        assert_eq!(client_ip(&headers, remote), "203.0.113.5", "xff ip");

        headers.insert("X-Forwarded-For", "203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&headers, remote), "203.0.113.9", "single xff ip");

        headers.remove("X-Forwarded-For");
        assert_eq!(
            client_ip_str(&headers, "bad-addr"),
            "bad-addr",
            "fallback ip"
        );

        // IPv6 peers lose their brackets like net.SplitHostPort; an unbracketed
        // IPv6 literal does not split and comes back verbatim.
        let v6: Option<SocketAddr> = Some("[2001:db8::1]:443".parse().unwrap());
        assert_eq!(client_ip(&headers, v6), "2001:db8::1");
        assert_eq!(client_ip_str(&headers, "2001:db8::1"), "2001:db8::1");
        assert_eq!(client_ip(&headers, None), "");
    }

    /// Verifies a burst larger than one batch is written completely and in
    /// arrival order.
    #[tokio::test]
    async fn recorder_batches_queued_events() {
        let (rec, store, _dir) = new_recorder().await;
        const N: usize = 600;
        for i in 0..N {
            rec.record(Event {
                repo: "npmjs".into(),
                action: meta::EVENT_DOWNLOAD.into(),
                path: format!("p{i}"),
                status: 200,
                ..Default::default()
            });
        }
        rec.close().await;

        let logs = store
            .list_audit_logs("npmjs", "", (N + 10) as i64, 0)
            .await
            .expect("list");
        assert_eq!(logs.len(), N, "len = {}, want {N}", logs.len());
        // Newest first: the last recorded path heads the list.
        assert!(
            logs[0].path == format!("p{}", N - 1) && logs[N - 1].path == "p0",
            "order lost: first={} last={}",
            logs[0].path,
            logs[N - 1].path
        );
    }
}
