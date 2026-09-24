//! Performs an approved tunnel replacement and watches it back to health. The
//! AWS call is irreversible, so this module cannot fix a bad outcome; it
//! separates "replaced and healthy" from "replaced and still down" and alerts
//! immediately if the surviving tunnel drops.

use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::aws::client::ApiError;
use crate::aws::types::since;
use crate::aws::{Connection, Tunnel};
use crate::humanize;

/// How a replacement ended. The values appear as a Prometheus metric label, so
/// they are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tunnel came back UP and carries routes.
    Succeeded,
    /// AWS accepted a dry-run call, nothing was replaced.
    DryRun,
    /// The call was rejected, nothing was replaced.
    RequestFailed,
    /// Replaced, but the tunnel never came back. Needs a human, since the
    /// replacement cannot be undone.
    VerifyTimeout,
    /// The surviving tunnel also dropped, so the connection lost both paths.
    PeerLost,
    /// Verification stopped on shutdown, but the replacement already happened.
    Aborted,
}

impl Outcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::DryRun => "dry_run",
            Self::RequestFailed => "request_failed",
            Self::VerifyTimeout => "verify_timeout",
            Self::PeerLost => "peer_lost",
            Self::Aborted => "aborted",
        }
    }

    /// Parses a persisted outcome. Unknown strings are not healthy, so a new
    /// outcome cannot silently become chainable.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "succeeded" => Some(Self::Succeeded),
            "dry_run" => Some(Self::DryRun),
            "request_failed" => Some(Self::RequestFailed),
            "verify_timeout" => Some(Self::VerifyTimeout),
            "peer_lost" => Some(Self::PeerLost),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    /// Whether the outcome needs no follow-up.
    #[must_use]
    pub const fn healthy(self) -> bool {
        matches!(self, Self::Succeeded | Self::DryRun)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Receives progress updates. The Slack implementation posts each one as a
/// reply in the approval thread. One method per severity, so a caller cannot
/// report a step without classifying it.
#[async_trait]
pub trait Reporter: Send + Sync {
    async fn info(&self, msg: String);
    async fn success(&self, msg: String);
    async fn warn(&self, msg: String);
    async fn error(&self, msg: String);
    async fn critical(&self, msg: String);
}

/// The AWS surface the executor needs.
#[async_trait]
pub trait VpnApi: Send + Sync {
    async fn replace(
        &self,
        connection_id: &str,
        outside_ip: &str,
        dry_run: bool,
    ) -> Result<(), ApiError>;
    async fn describe(&self, connection_id: &str) -> Result<Connection, ApiError>;
}

#[async_trait]
impl VpnApi for crate::aws::Client {
    async fn replace(
        &self,
        connection_id: &str,
        outside_ip: &str,
        dry_run: bool,
    ) -> Result<(), ApiError> {
        Self::replace(self, connection_id, outside_ip, dry_run).await
    }
    async fn describe(&self, connection_id: &str) -> Result<Connection, ApiError> {
        Self::describe(self, connection_id).await
    }
}

/// Called once AWS has answered that it accepted the request, and only then.
/// It is how the caller learns the moment a replacement is definitely under
/// way, which is a different fact from "the call was issued".
pub type OnAccepted = Box<dyn Fn() -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

/// One approved replacement.
pub struct Request {
    pub connection: Connection,
    /// The tunnel to replace. The outside IP survives a replacement, so it
    /// stays the identifier throughout verification.
    pub tunnel_ip: String,
    /// The tunnel expected to carry traffic meanwhile.
    pub peer_ip: String,
    /// Sends the AWS `DryRun` flag instead of really replacing.
    pub dry_run: bool,
    /// Recovers a replacement from persisted state, where the AWS call may
    /// already have happened and must not be repeated.
    pub resuming: bool,
    /// A resumed replacement whose AWS call was never seen to be accepted.
    pub acceptance_unknown: bool,
    /// When the replacement really began, set only on the resume path.
    pub started_at: Option<DateTime<Utc>>,
    pub on_accepted: Option<OnAccepted>,
}

impl Request {
    /// A fresh replacement.
    #[must_use]
    pub fn new(connection: Connection, tunnel_ip: &str, peer_ip: &str, dry_run: bool) -> Self {
        Self {
            connection,
            tunnel_ip: tunnel_ip.to_string(),
            peer_ip: peer_ip.to_string(),
            dry_run,
            resuming: false,
            acceptance_unknown: false,
            started_at: None,
            on_accepted: None,
        }
    }
}

/// The verification thresholds.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Bounds the wait for the tunnel to return.
    pub verify_timeout: Duration,
    /// The delay between telemetry reads.
    pub poll_interval: Duration,
    /// The route count a returned tunnel must carry to count as healthy.
    /// Ignored on static-routes-only connections.
    pub min_accepted_routes: i32,
    /// How often to report "still waiting" when nothing changed.
    pub heartbeat: Duration,
}

/// How a replacement ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome2 {
    pub outcome: Outcome,
    pub duration: Duration,
    pub detail: String,
    /// The surviving tunnel went down at some point, even if it recovered.
    pub peer_dropped: bool,
}

pub type ExecResult = Outcome2;

/// Runs replacements. Every invocation is one replacement; the caller owns
/// the shutdown token, which aborts verification without touching AWS.
pub struct Executor {
    api: Arc<dyn VpnApi>,
    opts: Options,
}

/// Performs approved replacements. The executor satisfies it; tests script it.
#[async_trait]
pub trait Replacer: Send + Sync {
    async fn run(
        &self,
        req: Request,
        reporter: &dyn Reporter,
        shutdown: CancellationToken,
    ) -> ExecResult;
}

impl Executor {
    #[must_use]
    pub fn new(api: Arc<dyn VpnApi>, opts: Options) -> Self {
        Self { api, opts }
    }

    /// Replaces the tunnel and verifies it, reporting each step. It returns a
    /// result rather than an error: once the tunnel is replaced, the question
    /// is what state it ended in, which an error return would lose.
    #[allow(clippy::too_many_lines)]
    pub async fn run(
        &self,
        req: Request,
        r: &dyn Reporter,
        shutdown: CancellationToken,
    ) -> ExecResult {
        let started = Instant::now();
        // `began` is when the replacement itself started, which after a restart
        // is earlier than now. Elapsed times are measured from it, while the
        // verification deadline still counts from now.
        let began = req.started_at.unwrap_or_else(Utc::now);
        let elapsed = || since(Utc::now(), began);
        let span = tracing::info_span!(
            "replacement",
            vpn_connection_id = %req.connection.id,
            tunnel_ip = %req.tunnel_ip,
            peer_ip = %req.peer_ip
        );
        let _enter = span.enter();

        if req.resuming {
            r.info(format!(
                "Controller restarted mid-replacement. Resuming verification of tunnel `{}` without re-issuing the AWS call. It has been replacing for {} so far.",
                req.tunnel_ip,
                humanize::elapsed(elapsed())
            ))
            .await;
            info!(
                acceptance_unknown = req.acceptance_unknown,
                elapsed = %humanize::elapsed(elapsed()),
                "resuming verification of an in-flight replacement"
            );
            let uncertain = req.acceptance_unknown;
            return self
                .verify(&req, r, started, began, uncertain, shutdown)
                .await;
        }

        if req.dry_run {
            r.info(format!(
                "Dry run in progress. Validating `ReplaceVpnTunnel` for tunnel `{}`.",
                req.tunnel_ip
            ))
            .await;
        } else {
            r.info(format!(
                "Calling `ReplaceVpnTunnel` on tunnel `{}`. It will drop shortly; traffic rides `{}`.",
                req.tunnel_ip, req.peer_ip
            ))
            .await;
        }

        match self
            .api
            .replace(&req.connection.id, &req.tunnel_ip, req.dry_run)
            .await
        {
            Err(ApiError::DryRunSucceeded) => {
                info!(elapsed = %humanize::elapsed(elapsed()), "dry run accepted by AWS; nothing was replaced");
                r.success(format!(
                    "Dry run accepted by AWS in {}. Permissions and arguments are valid; no tunnel was replaced.",
                    humanize::elapsed(elapsed())
                ))
                .await;
                ExecResult {
                    outcome: Outcome::DryRun,
                    duration: elapsed(),
                    detail: "AWS accepted the dry-run request; nothing was replaced".into(),
                    peer_dropped: false,
                }
            }
            Err(ApiError::Uncertain(err)) if req.dry_run => {
                // A dry run changes nothing whatever the transport did, so an
                // unanswered call is simply a failed dry run.
                error!(error = %err, elapsed = %humanize::elapsed(elapsed()), "dry-run ReplaceVpnTunnel did not return an answer");
                r.error(format!(
                    "The dry run did not get an answer from AWS after {}. Nothing was replaced.\n```{err}```",
                    humanize::elapsed(elapsed())
                ))
                .await;
                ExecResult {
                    outcome: Outcome::RequestFailed,
                    duration: elapsed(),
                    detail: err,
                    peer_dropped: false,
                }
            }
            Err(ApiError::Uncertain(err)) => {
                // AWS may have accepted the request and only the answer was
                // lost. Reporting "nothing was replaced" here is how a real
                // replacement ends up with nobody watching it.
                error!(error = %err, "ReplaceVpnTunnel did not return a definite answer; verifying instead of assuming it failed");
                r.warn(format!(
                    "AWS did not answer the replacement request, so it may or may not be under way. Watching tunnel `{}` as if it were (timeout {}).\n```{err}```",
                    req.tunnel_ip,
                    humanize::go_duration(self.opts.verify_timeout)
                ))
                .await;
                self.verify(&req, r, started, began, true, shutdown).await
            }
            Err(ApiError::Rejected(err)) => {
                error!(error = %err, elapsed = %humanize::elapsed(elapsed()), "ReplaceVpnTunnel was rejected");
                r.error(format!(
                    "AWS rejected the replacement request after {}. Nothing was replaced.\n```{err}```",
                    humanize::elapsed(elapsed())
                ))
                .await;
                ExecResult {
                    outcome: Outcome::RequestFailed,
                    duration: elapsed(),
                    detail: err,
                    peer_dropped: false,
                }
            }
            Ok(()) => {
                info!("ReplaceVpnTunnel accepted; verifying");
                // Reported before the first poll, so a crash in the next
                // instant leaves state saying the replacement is definitely
                // under way rather than merely requested.
                if let Some(on_accepted) = &req.on_accepted {
                    on_accepted().await;
                }
                r.info(format!(
                    "AWS accepted the replacement. Watching tunnel `{}` until it returns (timeout {}).",
                    req.tunnel_ip,
                    humanize::go_duration(self.opts.verify_timeout)
                ))
                .await;
                self.verify(&req, r, started, began, false, shutdown).await
            }
        }
    }

    /// Polls telemetry until the replaced tunnel is healthy or the timeout
    /// expires.
    ///
    /// Health requires two things: the tunnel is UP with enough routes, and it
    /// has actually cycled. Without the second condition an immediate poll
    /// could see the pre-replacement UP state and declare success before the
    /// tunnel had even dropped. `uncertain` marks a run whose AWS call was
    /// never answered, which only changes what a timeout means.
    #[allow(clippy::too_many_lines)]
    async fn verify(
        &self,
        req: &Request,
        r: &dyn Reporter,
        started: Instant,
        began: DateTime<Utc>,
        uncertain: bool,
        shutdown: CancellationToken,
    ) -> ExecResult {
        let started_wall = Utc::now();
        let deadline = started + self.opts.verify_timeout;
        let elapsed = || since(Utc::now(), began);
        let mut ticker = tokio::time::interval(self.opts.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        let mut saw_down = false;
        let mut peer_dropped = false;
        let mut peer_alerted = false;
        let mut last_reported = String::new();
        let mut last_heartbeat = Instant::now();

        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    warn!(elapsed = %humanize::elapsed(elapsed()), "verification aborted by shutdown; the replacement itself already happened");
                    r.warn(format!(
                        "Controller is shutting down while verifying tunnel `{}`, {} into the replacement. The replacement already happened; verification resumes when the controller comes back.",
                        req.tunnel_ip,
                        humanize::elapsed(elapsed())
                    ))
                    .await;
                    return ExecResult {
                        outcome: Outcome::Aborted,
                        duration: elapsed(),
                        detail: "verification interrupted by shutdown".into(),
                        peer_dropped,
                    };
                }
                _ = ticker.tick() => {}
            }

            let conn = match self.api.describe(&req.connection.id).await {
                Ok(c) => c,
                Err(err) => {
                    // A transient describe failure is not a verdict.
                    warn!(error = %err, "failed to read VPN telemetry while verifying; will retry");
                    if Instant::now() > deadline {
                        return self
                            .timeout(
                                req,
                                r,
                                began,
                                peer_dropped,
                                uncertain,
                                format!("telemetry could not be read. {err}"),
                            )
                            .await;
                    }
                    continue;
                }
            };

            let Some(target) = conn.tunnel(&req.tunnel_ip).cloned() else {
                // The outside IP survives an endpoint replacement, so losing it
                // means the connection itself changed under us.
                let msg = format!(
                    "tunnel {} is no longer reported by the connection",
                    req.tunnel_ip
                );
                error!(elapsed = %humanize::elapsed(elapsed()), "target tunnel disappeared during verification");
                r.error(format!(
                    "Tunnel `{}` is no longer reported by `{}`, {} into the replacement. The connection changed during the replacement.",
                    req.tunnel_ip,
                    conn.id,
                    humanize::elapsed(elapsed())
                ))
                .await;
                return ExecResult {
                    outcome: Outcome::VerifyTimeout,
                    duration: elapsed(),
                    detail: msg,
                    peer_dropped,
                };
            };
            let peer = conn.tunnel(&req.peer_ip).cloned();

            if !target.up {
                saw_down = true;
            }

            // The surviving tunnel dropping is the failure the preflight checks
            // exist to prevent, so it is reported the moment it is seen.
            if let Some(peer) = &peer {
                if !peer.up && !peer_alerted {
                    peer_dropped = true;
                    peer_alerted = true;
                    error!(
                        "peer tunnel went DOWN during the replacement; the connection has no healthy path"
                    );
                    r.critical(format!(
                        "*Peer tunnel `{}` just went DOWN while `{}` is being replaced.* This connection currently has no healthy tunnel. {}",
                        req.peer_ip,
                        req.tunnel_ip,
                        status_detail(peer)
                    ))
                    .await;
                }
                if peer.up && peer_alerted {
                    peer_alerted = false;
                    r.success(format!(
                        "Peer tunnel `{}` is back UP ({} route(s)).",
                        req.peer_ip, peer.accepted_routes
                    ))
                    .await;
                }
            }

            if self.healthy(&conn, &target, saw_down, started_wall) {
                let took = elapsed();
                info!(
                    elapsed = %humanize::elapsed(took),
                    elapsed_seconds = took.as_secs_f64(),
                    accepted_routes = target.accepted_routes,
                    "replacement verified"
                );
                r.success(format!(
                    "Tunnel `{}` is back UP with {} route(s). The replacement took {}.",
                    req.tunnel_ip,
                    target.accepted_routes,
                    humanize::elapsed(took)
                ))
                .await;
                return ExecResult {
                    outcome: Outcome::Succeeded,
                    duration: took,
                    detail: format!(
                        "tunnel UP with {} accepted route(s)",
                        target.accepted_routes
                    ),
                    peer_dropped,
                };
            }

            // Report on change, and on a heartbeat otherwise, so the thread
            // neither floods nor goes quiet during a long replacement.
            let summary = progress_line(&target, peer.as_ref(), elapsed());
            if summary != last_reported || last_heartbeat.elapsed() >= self.opts.heartbeat {
                r.info(summary.clone()).await;
                last_reported = summary;
                last_heartbeat = Instant::now();
            }

            if Instant::now() > deadline {
                return self
                    .timeout(
                        req,
                        r,
                        began,
                        peer_dropped,
                        uncertain,
                        "the tunnel never came back UP with enough accepted routes".into(),
                    )
                    .await;
            }
        }
    }

    /// Whether the replaced tunnel counts as recovered: UP, carrying enough
    /// routes, and demonstrably cycled since the replacement began.
    fn healthy(
        &self,
        conn: &Connection,
        target: &Tunnel,
        saw_down: bool,
        started: DateTime<Utc>,
    ) -> bool {
        if !target.up {
            return false;
        }
        if !conn.static_routes_only && target.accepted_routes < self.opts.min_accepted_routes {
            return false;
        }
        saw_down || target.last_status_change.is_some_and(|t| t > started)
    }

    async fn timeout(
        &self,
        req: &Request,
        r: &dyn Reporter,
        began: DateTime<Utc>,
        peer_dropped: bool,
        uncertain: bool,
        mut detail: String,
    ) -> ExecResult {
        let took = since(Utc::now(), began);
        error!(
            vpn_connection_id = %req.connection.id,
            tunnel_ip = %req.tunnel_ip,
            elapsed = %humanize::elapsed(took),
            elapsed_seconds = took.as_secs_f64(),
            uncertain,
            detail = %detail,
            "replacement verification timed out"
        );
        // The call was never answered, so an unchanged tunnel is the likely
        // case rather than a stuck replacement.
        let advice = if uncertain {
            detail.push_str(", and the AWS call was never answered, so it may never have started");
            "Check whether the tunnel still has pending maintenance before retrying. If it does, nothing was replaced."
        } else {
            "The replacement cannot be rolled back. Check the customer gateway side and the tunnel's IKE/IPsec status."
        };
        r.error(format!(
            "*Gave up on tunnel `{}` after {}.* {detail}\n{advice}",
            req.tunnel_ip,
            humanize::elapsed(took)
        ))
        .await;
        ExecResult {
            outcome: Outcome::VerifyTimeout,
            duration: took,
            detail,
            peer_dropped,
        }
    }
}

#[async_trait]
impl Replacer for Executor {
    async fn run(
        &self,
        req: Request,
        reporter: &dyn Reporter,
        shutdown: CancellationToken,
    ) -> ExecResult {
        Self::run(self, req, reporter, shutdown).await
    }
}

/// Renders the current state of both tunnels as sentences, since it is posted
/// to a Slack thread rather than to a log.
fn progress_line(target: &Tunnel, peer: Option<&Tunnel>, elapsed: Duration) -> String {
    let mut line = format!("Tunnel `{}` is {}.", target.outside_ip, up_down(target.up));
    if !target.status_message.is_empty() {
        line.push(' ');
        line.push_str(&status_detail(target));
    }
    if let Some(peer) = peer {
        let _ = write!(
            line,
            " Peer tunnel `{}` is {} with {} route(s).",
            peer.outside_ip,
            up_down(peer.up),
            peer.accepted_routes
        );
    }
    let _ = write!(line, " {} elapsed so far.", humanize::elapsed(elapsed));
    line
}

const fn up_down(up: bool) -> &'static str {
    if up { "UP" } else { "DOWN" }
}

fn status_detail(t: &Tunnel) -> String {
    if t.status_message.is_empty() {
        String::new()
    } else {
        format!("AWS reports {}.", t.status_message)
    }
}

#[cfg(test)]
pub mod testing {
    //! Scripted collaborators shared with the controller tests.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Records every reported line with its level.
    #[derive(Default)]
    pub struct Recorded {
        pub lines: Mutex<Vec<(String, String)>>,
    }

    impl Recorded {
        pub fn text(&self) -> String {
            self.lines
                .lock()
                .unwrap()
                .iter()
                .map(|(l, m)| format!("[{l}] {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[async_trait]
    impl Reporter for Recorded {
        async fn info(&self, msg: String) {
            self.lines.lock().unwrap().push(("INFO".into(), msg));
        }
        async fn success(&self, msg: String) {
            self.lines.lock().unwrap().push(("SUCCESS".into(), msg));
        }
        async fn warn(&self, msg: String) {
            self.lines.lock().unwrap().push(("WARN".into(), msg));
        }
        async fn error(&self, msg: String) {
            self.lines.lock().unwrap().push(("ERROR".into(), msg));
        }
        async fn critical(&self, msg: String) {
            self.lines.lock().unwrap().push(("CRITICAL".into(), msg));
        }
    }

    /// A scripted VPN API: `replace` answers from `replace_result`, and each
    /// `describe` pops the next connection, repeating the last one forever.
    #[derive(Default)]
    pub struct ScriptedVpn {
        pub replace_result: Mutex<Option<ApiError>>,
        pub replace_calls: Mutex<Vec<(String, String, bool)>>,
        pub describes: Mutex<VecDeque<Result<Connection, String>>>,
        pub describe_calls: Mutex<usize>,
    }

    impl ScriptedVpn {
        pub fn describing(conns: Vec<Result<Connection, String>>) -> Self {
            Self {
                describes: Mutex::new(conns.into_iter().collect()),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl VpnApi for ScriptedVpn {
        async fn replace(
            &self,
            connection_id: &str,
            outside_ip: &str,
            dry_run: bool,
        ) -> Result<(), ApiError> {
            self.replace_calls.lock().unwrap().push((
                connection_id.into(),
                outside_ip.into(),
                dry_run,
            ));
            match self.replace_result.lock().unwrap().as_ref() {
                None => Ok(()),
                Some(ApiError::Rejected(s)) => Err(ApiError::Rejected(s.clone())),
                Some(ApiError::Uncertain(s)) => Err(ApiError::Uncertain(s.clone())),
                Some(ApiError::DryRunSucceeded) => Err(ApiError::DryRunSucceeded),
            }
        }

        async fn describe(&self, _connection_id: &str) -> Result<Connection, ApiError> {
            *self.describe_calls.lock().unwrap() += 1;
            let mut q = self.describes.lock().unwrap();
            let next = if q.len() > 1 {
                q.pop_front()
            } else {
                q.front().cloned()
            };
            match next {
                Some(Ok(c)) => Ok(c),
                Some(Err(e)) => Err(ApiError::Uncertain(e)),
                None => Err(ApiError::Rejected("no scripted connection".into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::testing::*;
    use super::*;
    use crate::planner::fixtures::{connection, tunnel};

    fn opts() -> Options {
        Options {
            verify_timeout: Duration::from_millis(400),
            poll_interval: Duration::from_millis(20),
            min_accepted_routes: 1,
            heartbeat: Duration::from_millis(60),
        }
    }

    fn up_since(conn: &Connection, ip: &str, stable_secs: i64) -> Connection {
        let mut c = conn.clone();
        for t in &mut c.tunnels {
            if t.outside_ip == ip {
                *t = tunnel(ip, true, 4, stable_secs);
                t.last_status_change = Some(Utc::now() - chrono::TimeDelta::seconds(stable_secs));
            }
        }
        c
    }

    fn down(conn: &Connection, ip: &str) -> Connection {
        let mut c = conn.clone();
        for t in &mut c.tunnels {
            if t.outside_ip == ip {
                t.up = false;
                t.status_message = "IPSEC IS DOWN".into();
                t.accepted_routes = 0;
            }
        }
        c
    }

    #[test]
    fn outcome_helpers() {
        assert!(Outcome::Succeeded.healthy());
        assert!(Outcome::DryRun.healthy());
        assert!(!Outcome::VerifyTimeout.healthy());
        for o in [
            Outcome::Succeeded,
            Outcome::DryRun,
            Outcome::RequestFailed,
            Outcome::VerifyTimeout,
            Outcome::PeerLost,
            Outcome::Aborted,
        ] {
            assert_eq!(Outcome::parse(o.as_str()), Some(o));
            assert_eq!(o.to_string(), o.as_str());
        }
        assert_eq!(Outcome::parse("nope"), None);
    }

    #[tokio::test]
    async fn successful_replacement_is_verified_after_a_cycle() {
        let conn = connection("vpn-1");
        // Old UP, then DOWN, then UP again with routes.
        let api = Arc::new(ScriptedVpn::describing(vec![
            Ok(up_since(&conn, "1.1.1.1", 3600)),
            Ok(down(&conn, "1.1.1.1")),
            Ok(down(&conn, "1.1.1.1")),
            Ok(up_since(&conn, "1.1.1.1", 0)),
        ]));
        let exec = Executor::new(api.clone(), opts());
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted2 = accepted.clone();
        let mut req = Request::new(conn.clone(), "1.1.1.1", "2.2.2.2", false);
        req.on_accepted = Some(Box::new(move || {
            let a = accepted2.clone();
            Box::pin(async move {
                a.fetch_add(1, Ordering::SeqCst);
            })
        }));
        let rec = Recorded::default();
        let res = exec.run(req, &rec, CancellationToken::new()).await;
        assert_eq!(res.outcome, Outcome::Succeeded, "{}", rec.text());
        assert!(!res.peer_dropped);
        assert_eq!(res.detail, "tunnel UP with 4 accepted route(s)");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        assert_eq!(
            *api.replace_calls.lock().unwrap(),
            vec![("vpn-1".to_string(), "1.1.1.1".to_string(), false)]
        );
        let text = rec.text();
        assert!(
            text.contains("[INFO] Calling `ReplaceVpnTunnel` on tunnel `1.1.1.1`"),
            "{text}"
        );
        assert!(
            text.contains("[INFO] AWS accepted the replacement."),
            "{text}"
        );
        assert!(text.contains("Tunnel `1.1.1.1` is DOWN. AWS reports IPSEC IS DOWN. Peer tunnel `2.2.2.2` is UP with 4 route(s)."), "{text}");
        assert!(
            text.contains("[SUCCESS] Tunnel `1.1.1.1` is back UP with 4 route(s)."),
            "{text}"
        );
    }

    #[tokio::test]
    async fn recent_status_change_counts_as_a_cycle_without_seeing_down() {
        let conn = connection("vpn-1");
        // Never observed DOWN, but LastStatusChange is after the call.
        let mut fresh = up_since(&conn, "1.1.1.1", 0);
        fresh.tunnels[0].last_status_change = Some(Utc::now() + chrono::TimeDelta::seconds(5));
        let api = Arc::new(ScriptedVpn::describing(vec![Ok(fresh)]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::Succeeded, "{}", rec.text());
    }

    #[tokio::test]
    async fn dry_run_outcomes() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::default());
        *api.replace_result.lock().unwrap() = Some(ApiError::DryRunSucceeded);
        let exec = Executor::new(api.clone(), opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn.clone(), "1.1.1.1", "2.2.2.2", true),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::DryRun);
        assert!(
            rec.text().contains("[INFO] Dry run in progress."),
            "{}",
            rec.text()
        );
        assert!(
            rec.text().contains("[SUCCESS] Dry run accepted by AWS"),
            "{}",
            rec.text()
        );
        assert!(api.replace_calls.lock().unwrap()[0].2);

        *api.replace_result.lock().unwrap() = Some(ApiError::Uncertain("timeout".into()));
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", true),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::RequestFailed);
        assert_eq!(res.detail, "timeout");
        assert!(
            rec.text()
                .contains("[ERROR] The dry run did not get an answer from AWS"),
            "{}",
            rec.text()
        );
        assert_eq!(
            *api.describe_calls.lock().unwrap(),
            0,
            "a failed dry run is not verified"
        );
    }

    #[tokio::test]
    async fn rejection_is_final() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::default());
        *api.replace_result.lock().unwrap() =
            Some(ApiError::Rejected("InvalidParameterValue".into()));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::RequestFailed);
        assert_eq!(res.detail, "InvalidParameterValue");
        assert!(
            rec.text()
                .contains("[ERROR] AWS rejected the replacement request"),
            "{}",
            rec.text()
        );
    }

    #[tokio::test]
    async fn uncertain_call_is_verified_and_timeout_says_so() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::describing(vec![Ok(up_since(
            &conn, "1.1.1.1", 3600,
        ))]));
        *api.replace_result.lock().unwrap() = Some(ApiError::Uncertain("connection reset".into()));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::VerifyTimeout, "{}", rec.text());
        assert!(
            res.detail
                .ends_with("the AWS call was never answered, so it may never have started"),
            "{}",
            res.detail
        );
        let text = rec.text();
        assert!(
            text.contains("[WARN] AWS did not answer the replacement request"),
            "{text}"
        );
        assert!(
            text.contains(
                "Check whether the tunnel still has pending maintenance before retrying."
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn timeout_when_tunnel_never_returns_and_heartbeat_repeats() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::describing(vec![Ok(down(&conn, "1.1.1.1"))]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::VerifyTimeout);
        assert!(
            res.detail.starts_with("the tunnel never came back UP"),
            "{}",
            res.detail
        );
        let text = rec.text();
        assert!(
            text.contains("[ERROR] *Gave up on tunnel `1.1.1.1` after"),
            "{text}"
        );
        assert!(
            text.contains("The replacement cannot be rolled back."),
            "{text}"
        );
        let progress = rec
            .lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, m)| m.starts_with("Tunnel `1.1.1.1` is DOWN"))
            .count();
        assert!(
            progress >= 2,
            "heartbeat repeats unchanged progress: {text}"
        );
    }

    #[tokio::test]
    async fn peer_drop_is_critical_and_recorded() {
        let conn = connection("vpn-1");
        let both_down = down(&down(&conn, "1.1.1.1"), "2.2.2.2");
        let api = Arc::new(ScriptedVpn::describing(vec![
            Ok(down(&conn, "1.1.1.1")),
            Ok(both_down),
            Ok(down(&conn, "1.1.1.1")),
            Ok(up_since(&conn, "1.1.1.1", 0)),
        ]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::Succeeded, "{}", rec.text());
        assert!(res.peer_dropped);
        let text = rec.text();
        assert!(text.contains("[CRITICAL] *Peer tunnel `2.2.2.2` just went DOWN while `1.1.1.1` is being replaced.*"), "{text}");
        assert!(
            text.contains("[SUCCESS] Peer tunnel `2.2.2.2` is back UP (4 route(s))."),
            "{text}"
        );
    }

    #[tokio::test]
    async fn describe_failures_retry_until_deadline_and_missing_tunnel_ends() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::describing(vec![Err("throttled".into())]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn.clone(), "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::VerifyTimeout);
        assert!(
            res.detail
                .starts_with("telemetry could not be read. throttled"),
            "{}",
            res.detail
        );

        let mut gone = conn.clone();
        gone.tunnels.remove(0);
        let api = Arc::new(ScriptedVpn::describing(vec![Ok(gone)]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::VerifyTimeout);
        assert_eq!(
            res.detail,
            "tunnel 1.1.1.1 is no longer reported by the connection"
        );
        assert!(
            rec.text()
                .contains("The connection changed during the replacement."),
            "{}",
            rec.text()
        );
    }

    #[tokio::test]
    async fn resume_skips_the_call_and_shutdown_aborts() {
        let conn = connection("vpn-1");
        let api = Arc::new(ScriptedVpn::describing(vec![Ok(down(&conn, "1.1.1.1"))]));
        let exec = Executor::new(api.clone(), opts());
        let rec = Recorded::default();
        let shutdown = CancellationToken::new();
        let mut req = Request::new(conn, "1.1.1.1", "2.2.2.2", false);
        req.resuming = true;
        req.acceptance_unknown = true;
        req.started_at = Some(Utc::now() - chrono::TimeDelta::minutes(7));
        let stop = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            stop.cancel();
        });
        let res = exec.run(req, &rec, shutdown).await;
        assert_eq!(res.outcome, Outcome::Aborted);
        assert!(
            res.duration >= Duration::from_mins(7),
            "measured from the original start"
        );
        assert!(
            api.replace_calls.lock().unwrap().is_empty(),
            "no second AWS call on resume"
        );
        let text = rec.text();
        assert!(
            text.contains("[INFO] Controller restarted mid-replacement."),
            "{text}"
        );
        assert!(text.contains("It has been replacing for 7m"), "{text}");
        assert!(
            text.contains("[WARN] Controller is shutting down while verifying tunnel `1.1.1.1`"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn static_routes_ignore_route_count() {
        let mut conn = connection("vpn-1");
        conn.static_routes_only = true;
        let mut back = up_since(&conn, "1.1.1.1", 0);
        back.tunnels[0].accepted_routes = 0;
        let api = Arc::new(ScriptedVpn::describing(vec![
            Ok(down(&conn, "1.1.1.1")),
            Ok(back),
        ]));
        let exec = Executor::new(api, opts());
        let rec = Recorded::default();
        let res = exec
            .run(
                Request::new(conn, "1.1.1.1", "2.2.2.2", false),
                &rec,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(res.outcome, Outcome::Succeeded, "{}", rec.text());
    }

    #[test]
    fn progress_line_without_peer() {
        let t = Tunnel {
            outside_ip: "1.1.1.1".into(),
            up: true,
            ..Tunnel::default()
        };
        assert_eq!(
            progress_line(&t, None, Duration::from_secs(61)),
            "Tunnel `1.1.1.1` is UP. 1m 01s elapsed so far."
        );
    }
}
