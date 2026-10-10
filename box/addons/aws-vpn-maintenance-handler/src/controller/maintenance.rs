//! The approve-and-replace worker: posts the card, waits on the decision,
//! re-checks, replaces each tunnel of the connection in turn, and closes the
//! card with the outcome. Also the restart path that picks all of that back up
//! from persisted state.

use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::progress::RunPhase;
use super::reporter::ThreadReporter;
use super::{Controller, FINISH_TIMEOUT, history_from};
use crate::approval::Decision;
use crate::aws::Connection;
use crate::executor::{ExecResult, Outcome, Request};
use crate::humanize;
use crate::k8s::events::reason;
use crate::k8s::{Approval, InFlight, Phase, Snapshot};
use crate::planner::{self, Blocked, Candidate, Reason};
use crate::promx::Assessment;
use crate::slack::{Level, MessageRef, Notice, Proposal, approval_blocks, resolved_blocks};

/// How often an outstanding approval request is re-checked against the
/// preflight rules, so a card is withdrawn once no click could still succeed.
/// Thirty seconds is finer than the window boundary it exists to catch and
/// coarser than any telemetry that moves.
pub const REVALIDATE_INTERVAL: Duration = Duration::from_secs(30);

/// An approval request being waited on, freshly posted or adopted from
/// persisted state after a restart.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub request_id: String,
    pub connection_id: String,
    pub tunnel_ip: String,
    pub proposal: Proposal,
    pub refs: Vec<MessageRef>,
    /// What is left of the approval window, shorter for an adopted one.
    pub timeout: Duration,
}

/// The result of re-applying the preflight rules to one tunnel.
enum Recheck {
    Eligible(Box<Candidate>),
    /// `None` reason means the read itself failed, which is transient by
    /// nature.
    Blocked(Option<Reason>, String),
}

impl Controller {
    /// Posts an approval card for a fresh candidate.
    pub(super) async fn run_maintenance(
        &self,
        cand: Candidate,
        assessment: Assessment,
        shutdown: CancellationToken,
    ) {
        let span = tracing::info_span!(
            "maintenance",
            vpn_connection_id = %cand.connection.id,
            tunnel_ip = %cand.tunnel.outside_ip,
            request_id = %cand.request_id
        );
        let _enter = span.enter();

        let mut proposal = self.proposal(&cand);
        proposal.traffic_checked = assessment.evaluated;
        proposal.traffic_detail = assessment.detail;
        let (fallback, blocks) = approval_blocks(&proposal);
        let refs = self
            .slack
            .broadcast(&self.dm_channels, &fallback, &blocks)
            .await;
        if refs.is_empty() {
            // No reachable approver means no authorization path, so drop and
            // retry next pass.
            error!(
                "could not deliver the approval request to any approver; skipping this candidate"
            );
            self.metrics.observe_reconcile_error("approval_delivery");
            return;
        }

        if let Err(err) = self
            .store
            .add_approval(Approval {
                request_id: cand.request_id.clone(),
                posted_at: Some(Utc::now()),
                thread: refs.clone(),
            })
            .await
        {
            error!(error = %err, "failed to persist the outstanding approval request");
        }
        info!(
            approvers = refs.len(),
            escalated = cand.escalate,
            deadline_in = %humanize::go_duration(cand.deadline_in),
            "approval request sent"
        );
        self.events.normal(
            reason::APPROVAL_REQUESTED,
            format!(
                "Requested approval to replace tunnel {} of {} (AWS auto-applies in {})",
                cand.tunnel.outside_ip,
                cand.connection.label(),
                humanize::go_duration(humanize::round_to_minute(cand.deadline_in))
            ),
        );

        self.await_decision(
            PendingRequest {
                request_id: cand.request_id.clone(),
                connection_id: cand.connection.id.clone(),
                tunnel_ip: cand.tunnel.outside_ip.clone(),
                proposal,
                refs,
                timeout: self.cfg.approval.timeout.get(),
            },
            shutdown,
        )
        .await;
    }

    /// Blocks on the approver's answer and acts on it.
    ///
    /// The wait is not only a timeout. Preconditions lapse while a card sits in
    /// front of an approver, most often the window running out of room to
    /// still start and verify a replacement, so the loop re-applies the
    /// preflight rules and withdraws the card once no click could succeed any
    /// more. One registration covers the whole loop, so a click arriving
    /// between two re-checks is never dropped as no longer outstanding.
    pub(super) async fn await_decision(&self, req: PendingRequest, shutdown: CancellationToken) {
        let reporter = self.reporter(req.refs.clone(), &req.proposal.target());
        let mut watch = self.broker.watch(&req.request_id);

        let deadline = Utc::now() + chrono::TimeDelta::from_std(req.timeout).unwrap_or_default();
        let expiry = tokio::time::sleep(req.timeout);
        tokio::pin!(expiry);
        let mut revalidate = tokio::time::interval_at(
            Instant::now() + self.revalidate_interval,
            self.revalidate_interval,
        );

        loop {
            tokio::select! {
                decision = watch.recv() => {
                    if let Some(decision) = decision {
                        self.apply_decision(&req, decision, &reporter, shutdown).await;
                    }
                    return;
                }
                () = &mut expiry => {
                    let timeout = humanize::go_duration(self.cfg.approval.timeout.get());
                    self.resolve_without_replacing(&req, "timeout", Level::Warn, &format!(
                        "*Expired.* Nobody responded within {timeout}. The tunnel was left alone and will be proposed again in a later window."
                    )).await;
                    self.events.warning(reason::APPROVAL_TIMEOUT, format!(
                        "Approval to replace tunnel {} of {} expired after {timeout}",
                        req.tunnel_ip, req.connection_id
                    ));
                    return;
                }
                _ = revalidate.tick() => {
                    let Some(detail) = self.expiry_reason(&req, deadline).await else {
                        continue;
                    };
                    // A re-check takes several AWS calls, and a click during it
                    // has already been accepted by the broker and buffered. It
                    // was made against a card that was still live, so it wins
                    // over an expiry decided in the same moment.
                    if let Some(decision) = watch.try_recv() {
                        self.apply_decision(&req, decision, &reporter, shutdown).await;
                        return;
                    }
                    info!(detail = %detail, "approval request can no longer succeed; withdrawing the card");
                    self.resolve_without_replacing(&req, "expired", Level::Warn, &format!(
                        "*Expired.* {detail}. The tunnel was left alone and will be proposed again in a later window."
                    )).await;
                    self.events.warning(reason::APPROVAL_EXPIRED, format!(
                        "Approval to replace tunnel {} of {} expired before it could be answered: {detail}",
                        req.tunnel_ip, req.connection_id
                    ));
                    return;
                }
                () = shutdown.cancelled() => {
                    // Shutdown or lost leadership. The record stays so the next
                    // leader adopts the same card instead of posting a
                    // duplicate.
                    info!("stopped waiting for approval");
                    self.metrics.observe_approval("aborted");
                    return;
                }
            }
        }
    }

    /// Acts on an answered request.
    async fn apply_decision(
        &self,
        req: &PendingRequest,
        decision: Decision,
        reporter: &ThreadReporter,
        shutdown: CancellationToken,
    ) {
        if !decision.approved {
            self.resolve_without_replacing(
                req,
                "denied",
                Level::Warn,
                &format!(
                    "*Denied* by <@{}>. The tunnel was left alone.",
                    decision.user_id
                ),
            )
            .await;
            self.events.normal(
                reason::DENIED,
                format!(
                    "Replacement of tunnel {} of {} denied by {}",
                    req.tunnel_ip, req.connection_id, decision.user_name
                ),
            );
            return;
        }

        self.metrics.observe_approval("approved");
        info!(approver_user_id = %decision.user_id, approver = %decision.user_name, "replacement approved");
        // The run starts here, at the size the card announced. `execute`
        // corrects the count if the re-check finds the queue has changed.
        reporter.progress.start(req.proposal.queue.len() + 1);

        reporter
            .at(
                Level::Info,
                format!(
                    "Approved by <@{}>. Re-checking safety conditions before touching anything.",
                    decision.user_id
                ),
            )
            .await;
        self.events.normal(
            reason::APPROVED,
            format!(
                "Replacement of tunnel {} of {} approved by {}",
                req.tunnel_ip, req.connection_id, decision.user_name
            ),
        );

        self.execute(req, &decision.user_id, reporter, shutdown)
            .await;
        // The clock started with the approval rather than with the first AWS
        // call, because that is the moment the approver has been waiting from.
        self.report_run(reporter).await;
    }

    /// Posts the closing report of a run that replaced something, naming what
    /// the whole approval achieved and how long it took end to end.
    async fn report_run(&self, reporter: &ThreadReporter) {
        let elapsed = crate::aws::types::since(Utc::now(), reporter.progress.started_at());
        if let Some((level, summary)) = reporter.progress.report(elapsed) {
            reporter.at(level, summary).await;
        }
    }

    /// Why an outstanding request could no longer be acted on, or `None` to
    /// keep waiting.
    ///
    /// Three outcomes, because a failed re-check is not one thing. A read that
    /// failed says nothing about the tunnel and must not cost anyone their
    /// card. A block that cannot clear ends the request immediately. A block
    /// that can clear ends it only once clearing would come too late to be
    /// followed by a verified replacement.
    async fn expiry_reason(
        &self,
        req: &PendingRequest,
        deadline: chrono::DateTime<Utc>,
    ) -> Option<String> {
        let recheck = self
            .recheck_tunnel(&req.connection_id, &req.tunnel_ip)
            .await;

        // The window is the tighter of the two deadlines whenever the card went
        // up late in it.
        let now = Utc::now();
        let budget = crate::aws::types::until(now, deadline).min(self.window.start_budget(now));

        let cand = match recheck {
            Recheck::Blocked(None, _) => return None,
            Recheck::Blocked(Some(reason), detail) => {
                if !waitable(Some(reason)) {
                    return Some(detail);
                }
                if budget >= self.recovery_need(reason) {
                    return None;
                }
                return Some(out_of_time(&detail));
            }
            Recheck::Eligible(cand) => *cand,
        };

        // Every preflight rule passes, so traffic is the only thing left that
        // could stand in the way. It cannot decide anything until the budget is
        // down to the verification a replacement would need.
        if budget >= self.cfg.safety.verify_timeout.get() {
            return None;
        }
        let assessment = self.traffic.evaluate(&self.traffic_vars(&cand)).await;
        // Metrics that cannot be read say nothing about the tunnel, and an
        // outage of the monitoring stack must not quietly consume approvals.
        if assessment.allowed || !assessment.has_history {
            return None;
        }
        Some(out_of_time(&assessment.detail))
    }

    /// How long a clearable block still needs after it clears: a peer that
    /// comes back has to hold up for `peerMinStableFor` before the tunnel is a
    /// candidate again, and the replacement then needs `verifyTimeout`.
    fn recovery_need(&self, reason: Reason) -> Duration {
        match reason {
            Reason::PeerDown | Reason::PeerUnstable | Reason::PeerNoRoutes => {
                self.cfg.safety.peer_min_stable_for.get() + self.cfg.safety.verify_timeout.get()
            }
            _ => self.cfg.safety.verify_timeout.get(),
        }
    }

    /// Re-validates the preflight rules and then replaces. An approval can
    /// arrive an hour after the card was posted, and the peer tunnel may have
    /// dropped, started flapping, or lost its routes since.
    async fn execute(
        &self,
        req: &PendingRequest,
        approver_id: &str,
        reporter: &ThreadReporter,
        shutdown: CancellationToken,
    ) {
        let fresh = match self.recheck(&req.connection_id, &req.request_id).await {
            Recheck::Eligible(c) => *c,
            Recheck::Blocked(_, detail) => {
                warn!(detail = %detail, "preflight re-check failed after approval; not replacing");
                self.metrics.observe_reconcile_error("recheck");
                reporter
                    .at(
                        Level::Warn,
                        format!(
                            "*Not replacing.* Conditions changed between approval and execution.\n> {detail}\nNothing was touched. The tunnel will be proposed again once it is safe."
                        ),
                    )
                    .await;
                self.resolve_without_replacing(
                    req,
                    "aborted",
                    Level::Warn,
                    &format!("Aborted before any change. {detail}"),
                )
                .await;
                self.events.warning(
                    reason::HELD_BACK,
                    format!(
                        "Aborted approved replacement of tunnel {} of {} after re-check: {detail}",
                        req.tunnel_ip, req.connection_id
                    ),
                );
                return;
            }
        };

        // Traffic is re-measured too. It was quiet when the card was posted,
        // but a batch job may have started since.
        let assessment = self.traffic.evaluate(&self.traffic_vars(&fresh)).await;
        if assessment.evaluated {
            self.metrics.observe_traffic_gate(
                assessment.allowed,
                assessment.ratio,
                assessment.rank,
                assessment.has_history,
            );
        }
        if !assessment.allowed {
            warn!(detail = %assessment.detail, "traffic gate closed between approval and execution; not replacing");
            self.metrics.observe_blocked(Reason::TrafficHigh.as_str());
            reporter
                .at(
                    Level::Warn,
                    format!(
                        "*Not replacing.* The tunnel is no longer quiet.\n> {}\nNothing was touched. It will be proposed again once traffic drops.",
                        assessment.detail
                    ),
                )
                .await;
            self.resolve_without_replacing(
                req,
                "aborted",
                Level::Warn,
                &format!("Aborted before any change. {}", assessment.detail),
            )
            .await;
            self.events.warning(
                reason::HELD_BACK,
                format!(
                    "Aborted approved replacement of tunnel {} of {}: traffic gate closed ({})",
                    req.tunnel_ip, req.connection_id, assessment.detail
                ),
            );
            return;
        }

        // The queue is re-derived from fresh telemetry, so AWS may have queued
        // or applied maintenance since the card went up.
        reporter.progress.start(fresh.queue.len() + 1);
        if !fresh.queue.is_empty() {
            reporter
                .at(
                    Level::Info,
                    format!(
                        "This approval covers {} tunnel(s) of {}, replaced one at a time in this order.\n{}",
                        fresh.queue.len() + 1,
                        fresh.connection.label(),
                        chain_order(&fresh.tunnel.outside_ip, &fresh.queue)
                    ),
                )
                .await;
        }
        self.run_chain(req, fresh, approver_id, reporter, shutdown)
            .await;
    }

    /// Replaces each tunnel of the connection in turn under the one approval.
    ///
    /// Never two at once: the next tunnel waits until the previous one is a
    /// peer good enough to fail over to. Anything that would make the next step
    /// unsafe stops the chain and leaves the rest for a later window.
    async fn run_chain(
        &self,
        req: &PendingRequest,
        first: Candidate,
        approver_id: &str,
        reporter: &ThreadReporter,
        shutdown: CancellationToken,
    ) {
        let total = first.queue.len() + 1;
        announce_step(reporter, 1, total, &first.tunnel.outside_ip).await;

        let Some(result) = self
            .replace_one(
                req,
                &first,
                &first.queue,
                approver_id,
                0,
                reporter,
                shutdown.clone(),
            )
            .await
        else {
            return;
        };
        reporter.progress.record(0, result.outcome.healthy());
        let next = Self::waiting_record(
            req,
            &first.connection,
            &first.tunnel.outside_ip,
            &first.queue,
            approver_id,
            1,
            reporter,
        );
        self.complete_run(
            &first.connection,
            &first.tunnel.outside_ip,
            &result,
            &req.refs,
            &self.proposal(&first),
            next,
            reporter,
        )
        .await;

        if !result.outcome.healthy() {
            self.stop_chain(&first.queue, reporter).await;
            return;
        }
        self.continue_chain(
            req,
            &first.connection,
            first.queue.clone(),
            approver_id,
            reporter,
            total,
            1,
            shutdown,
        )
        .await;
    }

    /// The gap between two tunnels of one approved run: nothing is in flight at
    /// AWS, but the run is not over. `None` when this was the last tunnel.
    #[allow(clippy::too_many_arguments)]
    fn waiting_record(
        req: &PendingRequest,
        conn: &Connection,
        replaced: &str,
        queue: &[String],
        approver_id: &str,
        done: usize,
        reporter: &ThreadReporter,
    ) -> Option<InFlight> {
        let next = queue.first()?;
        Some(InFlight {
            request_id: req.request_id.clone(),
            connection_id: conn.id.clone(),
            tunnel_ip: next.clone(),
            // The tunnel just replaced becomes the peer of the next one.
            peer_ip: replaced.to_string(),
            phase: Phase::Waiting,
            started_at: Some(Utc::now()),
            run_started_at: Some(reporter.progress.started_at()),
            approved_by: approver_id.to_string(),
            thread: req.refs.clone(),
            queue: queue[1..].to_vec(),
            done,
        })
    }

    /// Works through the connection's remaining tunnels. Shared with the
    /// restart path, so a chain interrupted by a rollout finishes the same way
    /// it would have.
    #[allow(clippy::too_many_arguments)]
    async fn continue_chain(
        &self,
        req: &PendingRequest,
        conn: &Connection,
        mut queue: Vec<String>,
        approver_id: &str,
        reporter: &ThreadReporter,
        total: usize,
        mut done: usize,
        shutdown: CancellationToken,
    ) {
        while let Some(next) = queue.first().cloned() {
            let remaining = queue[1..].to_vec();
            reporter.progress.at(done, RunPhase::Checking);
            let cand = match self
                .await_chain_ready(&conn.id, &next, reporter, shutdown.clone())
                .await
            {
                Ok(c) => c,
                Err(detail) => {
                    reporter
                        .at(
                            Level::Warn,
                            format!(
                                "Stopping before tunnel `{next}`:\n> {detail}\nNothing further was touched; it will be proposed again in a later window."
                            ),
                        )
                        .await;
                    self.events.warning(
                        reason::HELD_BACK,
                        format!(
                            "Stopped the chain before tunnel {next} of {}: {detail}",
                            conn.label()
                        ),
                    );
                    // The waiting record only means "this run is not over".
                    // Leaving it behind would block every other connection.
                    self.drop_waiting().await;
                    return;
                }
            };

            announce_step(reporter, done + 1, total, &next).await;

            let Some(result) = self
                .replace_one(
                    req,
                    &cand,
                    &remaining,
                    approver_id,
                    done,
                    reporter,
                    shutdown.clone(),
                )
                .await
            else {
                self.drop_waiting().await;
                return;
            };
            reporter.progress.record(done, result.outcome.healthy());
            let waiting = Self::waiting_record(
                req,
                &cand.connection,
                &cand.tunnel.outside_ip,
                &remaining,
                approver_id,
                done + 1,
                reporter,
            );
            self.complete_run(
                &cand.connection,
                &cand.tunnel.outside_ip,
                &result,
                &req.refs,
                &self.proposal(&cand),
                waiting,
                reporter,
            )
            .await;

            if !result.outcome.healthy() {
                self.stop_chain(&remaining, reporter).await;
                return;
            }
            queue = remaining;
            done += 1;
        }
    }

    /// Clears a between-tunnels record for a run that ended early.
    async fn drop_waiting(&self) {
        match tokio::time::timeout(FINISH_TIMEOUT, self.store.clear_in_flight()).await {
            Ok(Ok(())) => self.metrics.set_in_flight(false),
            Ok(Err(err)) => {
                error!(error = %err, "failed to clear the between-tunnels record; the next pass will be blocked until it goes");
            }
            Err(_) => error!("timed out clearing the between-tunnels record"),
        }
    }

    /// Reports the tunnels left untouched after a step that did not end
    /// healthy.
    async fn stop_chain(&self, remaining: &[String], reporter: &ThreadReporter) {
        if remaining.is_empty() {
            return;
        }
        reporter
            .at(
                Level::Warn,
                format!(
                    "Stopping here: {} tunnel(s) of this connection still have maintenance pending, but the last replacement did not end healthy. They will be proposed again once the connection is healthy.",
                    remaining.len()
                ),
            )
            .await;
    }

    /// Picks up a replacement interrupted by a restart or leadership handover,
    /// before any new discovery.
    pub(super) async fn resume_in_flight(self: &std::sync::Arc<Self>, shutdown: CancellationToken) {
        let snap = match self.store.load().await {
            Ok(s) => s,
            Err(err) => {
                error!(error = %err, "failed to load persisted state on startup");
                return;
            }
        };
        let Some(f) = snap.in_flight.clone() else {
            self.adopt_approvals(&snap, shutdown);
            return;
        };

        let total = f.done + 1 + f.queue.len();
        let started = f.started_at.unwrap_or_else(Utc::now);
        warn!(
            vpn_connection_id = %f.connection_id,
            tunnel_ip = %f.tunnel_ip,
            request_id = %f.request_id,
            phase = %f.phase,
            started_at = %started.to_rfc3339(),
            elapsed = %humanize::elapsed(crate::aws::types::since(Utc::now(), started)),
            approved_by = %f.approved_by,
            done = f.done,
            queued = f.queue.len(),
            "found an interrupted approved run; picking it up"
        );

        let conn = match self.aws.describe(&f.connection_id).await {
            Ok(c) => c,
            Err(err) => {
                error!(vpn_connection_id = %f.connection_id, error = %err, "failed to describe the connection of the interrupted replacement");
                self.metrics.observe_reconcile_error("resume_describe");
                return;
            }
        };
        if self
            .busy
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return;
        }
        self.metrics.set_in_flight(true);

        let req = PendingRequest {
            request_id: f.request_id.clone(),
            connection_id: f.connection_id.clone(),
            tunnel_ip: f.tunnel_ip.clone(),
            refs: f.thread.clone(),
            proposal: self.proposal_from_in_flight(&conn, &f),
            timeout: Duration::ZERO,
        };

        let this = self.clone();
        tokio::spawn(async move {
            this.resume_run(req, conn, f, total, shutdown).await;
            this.busy.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    async fn resume_run(
        &self,
        req: PendingRequest,
        conn: Connection,
        f: InFlight,
        total: usize,
        shutdown: CancellationToken,
    ) {
        let reporter = self.reporter(f.thread.clone(), &crate::slack::label(&conn.name, &conn.id));
        // The run start falls back to this tunnel's own start for a record
        // written before the run clock existed.
        let run_start = f.run_started_at.or(f.started_at);

        // Nothing was in flight at AWS: the run was between tunnels, waiting
        // for the one just replaced to become a peer worth failing over to.
        if f.phase == Phase::Waiting {
            let mut remaining = vec![f.tunnel_ip.clone()];
            remaining.extend(f.queue.iter().cloned());
            reporter
                .progress
                .resume(total, f.done, RunPhase::Checking, run_start);
            reporter
                .at(
                    Level::Info,
                    format!(
                        "Picking the approved run back up after a restart. {} of {total} tunnel(s) are done, and the rest follow in this order.\n{}",
                        f.done,
                        chain_order(&remaining[0], &remaining[1..])
                    ),
                )
                .await;
            self.continue_chain(
                &req,
                &conn,
                remaining,
                &f.approved_by,
                &reporter,
                total,
                f.done,
                shutdown,
            )
            .await;
            self.report_run(&reporter).await;
            return;
        }

        // The AWS call already happened, so this process picks the run up in
        // the verifying phase rather than at the start of the tunnel.
        reporter
            .progress
            .resume(total, f.done, RunPhase::Verifying, run_start);
        let mut request = Request::new(conn.clone(), &f.tunnel_ip, &f.peer_ip, self.cfg.dry_run);
        request.resuming = true;
        request.started_at = f.started_at;
        // A record still in the requested phase means nobody ever saw AWS
        // accept the call.
        request.acceptance_unknown = f.phase == Phase::Requested;
        let result = self.exec.run(request, &reporter, shutdown.clone()).await;
        reporter.progress.record(f.done, result.outcome.healthy());
        let waiting = Self::waiting_record(
            &req,
            &conn,
            &f.tunnel_ip,
            &f.queue,
            &f.approved_by,
            f.done + 1,
            &reporter,
        );
        self.complete_run(
            &conn,
            &f.tunnel_ip,
            &result,
            &f.thread,
            &req.proposal,
            waiting,
            &reporter,
        )
        .await;

        // The approval covered the whole connection, so a restart mid-chain
        // has to finish it.
        if f.queue.is_empty() || !result.outcome.healthy() {
            self.stop_chain(&f.queue, &reporter).await;
        } else {
            reporter
                .at(
                    Level::Info,
                    format!(
                        "Continuing the approved run. {} of {total} tunnel(s) are done, and the rest follow in this order.\n{}",
                        f.done + 1,
                        chain_order(&f.queue[0], &f.queue[1..])
                    ),
                )
                .await;
            self.continue_chain(
                &req,
                &conn,
                f.queue.clone(),
                &f.approved_by,
                &reporter,
                total,
                f.done + 1,
                shutdown,
            )
            .await;
        }
        self.report_run(&reporter).await;
    }

    /// Takes over a request still outstanding when the previous leader
    /// stopped. The existing card is still clickable, so re-posting would leave
    /// two in the DM with only one of them wired up.
    fn adopt_approvals(self: &std::sync::Arc<Self>, snap: &Snapshot, shutdown: CancellationToken) {
        for (id, rec) in &snap.approvals {
            let elapsed = rec
                .posted_at
                .map_or(Duration::MAX, |t| crate::aws::types::since(Utc::now(), t));
            let remaining = self.cfg.approval.timeout.get().saturating_sub(elapsed);
            let parsed = planner::split_request_id(id);
            let parsable = parsed.is_some();
            let Some((connection_id, tunnel_ip)) =
                parsed.filter(|_| !remaining.is_zero() && !rec.thread.is_empty())
            else {
                info!(
                    request_id = %id,
                    remaining = %humanize::go_duration(remaining),
                    parsable,
                    threads = rec.thread.len(),
                    "dropping a stale recorded approval request"
                );
                let store = self.store.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    if let Err(err) = store.remove_approval(&id).await {
                        error!(request_id = %id, error = %err, "failed to drop the stale approval request");
                    }
                });
                continue;
            };
            if self
                .busy
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                return;
            }
            info!(
                request_id = %id,
                vpn_connection_id = %connection_id,
                tunnel_ip = %tunnel_ip,
                remaining = %humanize::go_duration(remaining),
                "adopting an outstanding approval request from persisted state"
            );
            let req = PendingRequest {
                request_id: id.clone(),
                connection_id: connection_id.clone(),
                tunnel_ip: tunnel_ip.clone(),
                refs: rec.thread.clone(),
                timeout: remaining,
                proposal: Proposal {
                    request_id: id.clone(),
                    connection_id,
                    tunnel_ip,
                    region: self.cfg.region.clone(),
                    dry_run: self.cfg.dry_run,
                    approval_expiry: remaining,
                    window: self.window.to_string(),
                    ..Proposal::default()
                },
            };
            let this = self.clone();
            tokio::spawn(async move {
                this.await_decision(req, shutdown).await;
                this.busy.store(false, std::sync::atomic::Ordering::SeqCst);
            });
            // One at a time: a decision leads straight into the single
            // replacement slot.
            return;
        }
    }

    /// Records the outcome of one step, clears the in-flight record, and closes
    /// the card. An aborted run is the one case the record is kept: the
    /// replacement really happened and is still unverified, so the next leader
    /// must resume it rather than find a clean slate.
    #[allow(clippy::too_many_arguments)]
    async fn complete_run(
        &self,
        conn: &Connection,
        tunnel_ip: &str,
        result: &ExecResult,
        refs: &[MessageRef],
        proposal: &Proposal,
        next: Option<InFlight>,
        reporter: &ThreadReporter,
    ) {
        self.metrics.observe_replacement(
            result.outcome.as_str(),
            result.duration,
            result.peer_dropped,
        );

        if result.outcome == Outcome::Aborted {
            warn!(
                elapsed = %humanize::elapsed(result.duration),
                elapsed_seconds = result.duration.as_secs_f64(),
                "replacement left in-flight for the next leader to verify"
            );
            return;
        }

        // A healthy tunnel with more to come hands the record straight to the
        // next step rather than clearing it.
        let write = async {
            match next {
                Some(next) if result.outcome.healthy() => {
                    if let Err(err) = self
                        .store
                        .advance_chain(
                            &conn.id,
                            tunnel_ip,
                            result.outcome.as_str(),
                            Utc::now(),
                            next,
                        )
                        .await
                    {
                        error!(error = %err, "failed to record the next tunnel of the approved run");
                    }
                }
                _ => {
                    if let Err(err) = self
                        .store
                        .finish_in_flight(&conn.id, tunnel_ip, result.outcome.as_str(), Utc::now())
                        .await
                    {
                        error!(error = %err, "failed to clear in-flight state");
                    }
                    self.metrics.set_in_flight(false);
                }
            }
        };
        if tokio::time::timeout(FINISH_TIMEOUT, write).await.is_err() {
            error!("timed out recording the replacement outcome");
        }

        if result.peer_dropped {
            self.events.warning(
                reason::PEER_LOST,
                format!(
                    "Peer tunnel dropped while replacing tunnel {tunnel_ip} of {}; the connection had no healthy path",
                    conn.label()
                ),
            );
        }
        let took = humanize::elapsed(result.duration);
        if result.outcome.healthy() {
            info!(outcome = %result.outcome, took = %took, took_seconds = result.duration.as_secs_f64(), "replacement finished");
            self.events.normal(
                reason::REPLACED,
                format!(
                    "Tunnel {tunnel_ip} of {}: {} ({}) after {took}",
                    conn.label(),
                    result.outcome,
                    result.detail
                ),
            );
        } else {
            error!(outcome = %result.outcome, detail = %result.detail, took = %took, took_seconds = result.duration.as_secs_f64(), "replacement finished badly");
            self.events.warning(
                reason::REPLACE_FAILED,
                format!(
                    "Tunnel {tunnel_ip} of {}: {} ({}) after {took}",
                    conn.label(),
                    result.outcome,
                    result.detail
                ),
            );
        }

        let (level, summary) = outcome_summary(result);
        self.close_card(
            proposal,
            refs,
            level,
            &reporter.progress.with_progress(&summary),
        )
        .await;
    }

    /// Posts the closing line and rewrites the card without its buttons. Both
    /// carry the outcome's level, so the resolved card no longer reads as a
    /// pending action even when it is found weeks later.
    async fn close_card(
        &self,
        proposal: &Proposal,
        refs: &[MessageRef],
        level: Level,
        summary: &str,
    ) {
        let close = async {
            self.slack
                .reply(
                    refs,
                    &Notice {
                        level,
                        target: crate::slack::label(
                            &proposal.connection_name,
                            &proposal.connection_id,
                        ),
                        text: summary.to_string(),
                    },
                )
                .await;
            let (fallback, blocks) = resolved_blocks(proposal, level, summary);
            self.slack.update(refs, &fallback, &blocks).await;
        };
        if tokio::time::timeout(FINISH_TIMEOUT, close).await.is_err() {
            error!("timed out closing the approval card");
        }
    }

    /// Closes a card nothing is waiting on any more. The approver already
    /// clicked, so leaving the buttons in place would produce a card that
    /// looks live and silently does nothing when pressed. Separate from
    /// `resolve_without_replacing` because the approval itself was answered;
    /// counting it again would double-count the decision.
    async fn abandon_request(&self, req: &PendingRequest) {
        self.close_card(
            &req.proposal,
            &req.refs,
            Level::Error,
            "*Closed without replacing anything.* The controller could not record what it was about to do. The tunnel is proposed again in a later window.",
        )
        .await;
        if let Err(err) = self.store.remove_approval(&req.request_id).await {
            error!(error = %err, "failed to drop the recorded approval request");
        }
    }

    /// Closes a request that never reached the AWS call.
    async fn resolve_without_replacing(
        &self,
        req: &PendingRequest,
        decision: &str,
        level: Level,
        summary: &str,
    ) {
        self.metrics.observe_approval(decision);
        info!(decision, request_id = %req.request_id, "approval request resolved without replacing");
        self.close_card(&req.proposal, &req.refs, level, summary)
            .await;
        if let Ok(Err(err)) =
            tokio::time::timeout(FINISH_TIMEOUT, self.store.remove_approval(&req.request_id)).await
        {
            error!(error = %err, "failed to drop the recorded approval request");
        }
    }

    /// Performs a single step of the chain: record it, call AWS, verify. The
    /// in-flight record is written before the AWS call and carries the rest of
    /// the queue, so a crash leaves both facts visible.
    #[allow(clippy::too_many_arguments)]
    async fn replace_one(
        &self,
        req: &PendingRequest,
        step: &Candidate,
        queue: &[String],
        approver_id: &str,
        done: usize,
        reporter: &ThreadReporter,
        shutdown: CancellationToken,
    ) -> Option<ExecResult> {
        reporter.progress.at(done, RunPhase::Replacing);
        if let Err(err) = self
            .store
            .set_in_flight(InFlight {
                request_id: step.request_id.clone(),
                connection_id: step.connection.id.clone(),
                tunnel_ip: step.tunnel.outside_ip.clone(),
                peer_ip: step.peer.outside_ip.clone(),
                phase: Phase::Requested,
                started_at: Some(Utc::now()),
                run_started_at: Some(reporter.progress.started_at()),
                approved_by: approver_id.to_string(),
                thread: req.refs.clone(),
                queue: queue.to_vec(),
                done,
            })
            .await
        {
            error!(error = %err, "failed to persist in-flight state; refusing to replace");
            self.metrics.observe_reconcile_error("persist_in_flight");
            reporter.at(Level::Error, format!(
                "*Not replacing tunnel `{}`.* Could not record the in-flight state in the ConfigMap, and a replacement that cannot be tracked must not be started. Nothing was touched. This request is closed rather than left clickable, because nothing is waiting on it any more; the tunnel is proposed again in a later window.\n```{err}```",
                step.tunnel.outside_ip
            )).await;
            self.abandon_request(req).await;
            self.events.warning(
                reason::HELD_BACK,
                format!(
                    "Refused to replace tunnel {} of {}: the in-flight state could not be recorded ({err})",
                    step.tunnel.outside_ip,
                    step.connection.label()
                ),
            );
            return None;
        }
        self.metrics.set_in_flight(true);
        self.events.normal(
            reason::REPLACING,
            format!(
                "Replacing tunnel {} of {} (approved by {approver_id}, dry_run={})",
                step.tunnel.outside_ip,
                step.connection.label(),
                self.cfg.dry_run
            ),
        );

        let mut request = Request::new(
            step.connection.clone(),
            &step.tunnel.outside_ip,
            &step.peer.outside_ip,
            self.cfg.dry_run,
        );
        // Recorded only once AWS has answered that it accepted the call.
        // Advancing the phase before that would erase the one distinction the
        // phase exists to keep.
        let store = self.store.clone();
        let progress = reporter.progress.clone();
        request.on_accepted = Some(Box::new(move || {
            let store = store.clone();
            let progress = progress.clone();
            Box::pin(async move {
                progress.at(done, RunPhase::Verifying);
                if let Err(err) = store.set_phase(Phase::Verifying).await {
                    warn!(error = %err, "failed to record the verifying phase");
                }
            })
        }));
        Some(self.exec.run(request, reporter, shutdown).await)
    }

    /// Waits until the next tunnel of the connection passes every preflight
    /// check, including the peer check against the tunnel that was just
    /// replaced. Right after a replacement the previous tunnel is UP but too
    /// young to be trusted, and that resolves on its own within
    /// `peerMinStableFor`. It gives up when the wait could no longer be
    /// followed by a verified replacement inside the window.
    ///
    /// Traffic is measured but does not gate a chained step: leaving a
    /// connection half-replaced means a second window, a second approval, and
    /// a second failover for the same work, and the peer check already proves
    /// the freshly replaced tunnel can carry traffic.
    async fn await_chain_ready(
        &self,
        connection_id: &str,
        tunnel_ip: &str,
        reporter: &ThreadReporter,
        shutdown: CancellationToken,
    ) -> Result<Candidate, String> {
        let deadline = Instant::now()
            + self.cfg.safety.peer_min_stable_for.get()
            + self.cfg.safety.verify_timeout.get();
        let mut ticker = tokio::time::interval(self.cfg.safety.verify_poll_interval.get());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        let mut announced = false;
        loop {
            let why = match self.recheck_tunnel(connection_id, tunnel_ip).await {
                Recheck::Eligible(cand) => {
                    self.note_chain_traffic(&cand, reporter).await;
                    info!(tunnel_ip, "next tunnel in the chain is ready");
                    return Ok(*cand);
                }
                Recheck::Blocked(reason, why) if !waitable(reason) => {
                    info!(tunnel_ip, reason = reason.map_or("", Reason::as_str), detail = %why, "the next tunnel in the chain cannot become ready by waiting");
                    return Err(why);
                }
                Recheck::Blocked(_, why) => why,
            };

            if !announced {
                reporter
                    .at(
                        Level::Info,
                        format!("Waiting before tunnel `{tunnel_ip}`: {why}"),
                    )
                    .await;
                announced = true;
            }
            info!(tunnel_ip, detail = %why, "waiting for the next tunnel in the chain");

            if Instant::now() > deadline {
                return Err(why);
            }
            tokio::select! {
                () = shutdown.cancelled() => return Err("controller is shutting down".to_string()),
                _ = ticker.tick() => {}
            }
        }
    }

    /// Measures a chained step's traffic without letting it stop the step. The
    /// metric is still recorded, and an elevated reading is said out loud in
    /// the thread so an operator reading it later does not have to infer that
    /// the second tunnel went ahead during a busy moment.
    async fn note_chain_traffic(&self, cand: &Candidate, reporter: &ThreadReporter) {
        let assessment = self.traffic.evaluate(&self.traffic_vars(cand)).await;
        if !assessment.evaluated {
            return;
        }
        self.metrics.observe_traffic_gate(
            assessment.allowed,
            assessment.ratio,
            assessment.rank,
            assessment.has_history,
        );
        if assessment.allowed {
            return;
        }
        info!(detail = %assessment.detail, "continuing the approved run through elevated traffic");
        reporter
            .at(
                Level::Warn,
                format!(
                    "Tunnel `{}` is not quiet, but this run already replaced its peer and continues anyway.\n> {}\nThe peer is UP and stable, so the failover still has a healthy path.",
                    cand.tunnel.outside_ip, assessment.detail
                ),
            )
            .await;
    }

    /// Re-reads the connection and re-applies every preflight rule, returning
    /// the refreshed candidate when the same request is still eligible.
    async fn recheck(&self, connection_id: &str, request_id: &str) -> Recheck {
        self.recheck_matching(
            connection_id,
            |c| c.request_id == request_id,
            |b| planner::request_id_matches(request_id, &b.connection_id, &b.tunnel_ip),
        )
        .await
    }

    /// Re-applies every preflight rule to one tunnel by its outside IP. The
    /// chain needs this because a queued tunnel has its own request ID, derived
    /// from its own maintenance deadline, which the approval never carried.
    async fn recheck_tunnel(&self, connection_id: &str, tunnel_ip: &str) -> Recheck {
        self.recheck_matching(
            connection_id,
            |c| c.tunnel.outside_ip == tunnel_ip,
            |b| b.tunnel_ip == tunnel_ip,
        )
        .await
    }

    async fn recheck_matching(
        &self,
        connection_id: &str,
        wanted: impl Fn(&Candidate) -> bool,
        rejected: impl Fn(&Blocked) -> bool,
    ) -> Recheck {
        let conn = match self.aws.describe(connection_id).await {
            Ok(c) => c,
            Err(err) => {
                return Recheck::Blocked(
                    None,
                    format!("the VPN connection could not be re-read. {err}"),
                );
            }
        };
        let statuses = match self.aws.statuses(&conn).await {
            Ok(s) => s,
            Err(err) => {
                return Recheck::Blocked(
                    None,
                    format!("tunnel maintenance status could not be re-read. {err}"),
                );
            }
        };
        let snap = match self.store.load().await {
            Ok(s) => s,
            Err(err) => {
                return Recheck::Blocked(
                    None,
                    format!("controller state could not be re-read. {err}"),
                );
            }
        };

        let now = Utc::now();
        let (open, window_detail) = self.window.open(now);
        let mut statuses_map = std::collections::HashMap::new();
        statuses_map.insert(conn.id.clone(), statuses);
        let plan = planner::evaluate(&planner::Input {
            now,
            connections: vec![conn],
            statuses: statuses_map,
            window_open: open,
            window_detail,
            // This run holds the single replacement slot, so it must not block
            // itself.
            replacement_in_flight: false,
            awaiting_approval: std::collections::HashSet::new(),
            history: history_from(&snap),
            thresholds: self.thresholds(),
        });

        if let Some(cand) = plan.candidates.iter().find(|c| wanted(c)) {
            return Recheck::Eligible(Box::new(cand.clone()));
        }
        if let Some(b) = plan.blocked.iter().find(|b| rejected(b)) {
            return Recheck::Blocked(Some(b.reason), b.detail.clone());
        }
        Recheck::Blocked(
            Some(Reason::TunnelCount),
            "the tunnel is no longer reported by this connection".to_string(),
        )
    }

    pub(super) fn reporter(&self, refs: Vec<MessageRef>, target: &str) -> ThreadReporter {
        ThreadReporter::new(self.slack.clone(), refs, target)
    }
}

/// Marks where in the announced order the thread currently is. A single-tunnel
/// approval gets no banner: numbering one step reads as noise.
async fn announce_step(reporter: &ThreadReporter, step: usize, total: usize, tunnel_ip: &str) {
    if total < 2 {
        return;
    }
    reporter
        .at(
            Level::Info,
            format!("*Step {step} of {total}.* Tunnel `{tunnel_ip}` is next."),
        )
        .await;
}

/// Phrases a block that could still clear but no longer soon enough.
fn out_of_time(detail: &str) -> String {
    format!(
        "{detail}, and too little time is left for that to clear and the replacement still be verified"
    )
}

/// Renders the replacement order as a numbered list, matching the one on the
/// approval card.
fn chain_order(first: &str, queue: &[String]) -> String {
    let mut lines = vec![format!("1. `{first}`")];
    for (i, ip) in queue.iter().enumerate() {
        lines.push(format!("{}. `{ip}`", i + 2));
    }
    lines.join("\n")
}

/// Whether a blocked reason could clear while the chain waits for it. The
/// waiting loop holds the single replacement slot, so waiting on something
/// that cannot resolve would idle every other connection for the better part
/// of an hour and then give up anyway.
const fn waitable(reason: Option<Reason>) -> bool {
    matches!(
        reason,
        None | Some(
            Reason::PeerDown | Reason::PeerUnstable | Reason::PeerNoRoutes | Reason::TrafficHigh
        )
    )
}

/// Renders the closing line of a run together with its level. Every branch
/// ends with how long the step took, including the ones that failed: the time
/// an unsuccessful replacement burned is what tells an operator whether the
/// window still has room for the rest of the connection.
fn outcome_summary(r: &ExecResult) -> (Level, String) {
    let took = humanize::elapsed(r.duration);
    match r.outcome {
        Outcome::Succeeded => {
            let s = format!("*Replaced.* {} in {took}.", r.detail);
            if r.peer_dropped {
                (
                    Level::Warn,
                    format!(
                        "{s}\nThe peer tunnel also dropped during the replacement, so the connection was briefly without a healthy path. Worth reviewing."
                    ),
                )
            } else {
                (Level::Success, s)
            }
        }
        Outcome::DryRun => (
            Level::Success,
            format!("*Dry run complete.* {}. Took {took}.", r.detail),
        ),
        Outcome::RequestFailed => (
            Level::Error,
            format!(
                "*Rejected by AWS after {took}.* Nothing was replaced. {}",
                r.detail
            ),
        ),
        Outcome::VerifyTimeout => (
            Level::Error,
            format!(
                "*Replaced but not healthy after {took}.* {}. This needs a human because the replacement cannot be undone.",
                r.detail
            ),
        ),
        Outcome::PeerLost => (
            Level::Critical,
            format!(
                "*Both tunnels were down during the replacement.* {}. It ran for {took}.",
                r.detail
            ),
        ),
        Outcome::Aborted => (
            Level::Warn,
            format!(
                "Replacement ended with outcome {} after {took}. {}",
                r.outcome, r.detail
            ),
        ),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn chain_order_and_waitable() {
        assert_eq!(
            chain_order("a", &["b".into(), "c".into()]),
            "1. `a`\n2. `b`\n3. `c`"
        );
        assert_eq!(chain_order("a", &[]), "1. `a`");
        assert!(waitable(None));
        assert!(waitable(Some(Reason::PeerUnstable)));
        assert!(!waitable(Some(Reason::WindowClosed)));
        assert!(!waitable(Some(Reason::NoPendingMaintenance)));
        assert_eq!(
            out_of_time("x"),
            "x, and too little time is left for that to clear and the replacement still be verified"
        );
    }

    #[test]
    fn outcome_summaries() {
        let r = |outcome, peer_dropped| ExecResult {
            outcome,
            duration: Duration::from_secs(65),
            detail: "d".into(),
            peer_dropped,
        };
        assert_eq!(
            outcome_summary(&r(Outcome::Succeeded, false)),
            (Level::Success, "*Replaced.* d in 1m 05s.".into())
        );
        let (level, s) = outcome_summary(&r(Outcome::Succeeded, true));
        assert_eq!(level, Level::Warn);
        assert!(s.contains("peer tunnel also dropped"));
        assert_eq!(
            outcome_summary(&r(Outcome::DryRun, false)).1,
            "*Dry run complete.* d. Took 1m 05s."
        );
        assert_eq!(
            outcome_summary(&r(Outcome::RequestFailed, false)).0,
            Level::Error
        );
        assert!(
            outcome_summary(&r(Outcome::VerifyTimeout, false))
                .1
                .starts_with("*Replaced but not healthy after 1m 05s.*")
        );
        assert_eq!(
            outcome_summary(&r(Outcome::PeerLost, false)).0,
            Level::Critical
        );
        assert!(
            outcome_summary(&r(Outcome::Aborted, false))
                .1
                .starts_with("Replacement ended with outcome aborted")
        );
    }
}
