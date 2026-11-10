//! Periodic AWS Health API poller.
//!
//! Two concurrent responsibilities run in the same cycle:
//!
//! 1. **Notifications** — events whose `lastUpdatedTime` advanced since the
//!    previous poll get forwarded once per state change.
//! 2. **Reminders** — for every `reminder_offset_hours` value, we re-check
//!    upcoming events; when `startTime - now <= offset` and we haven't fired
//!    that `(arn, offset)` pair yet, we post a reminder message.
//!
//! Only the leader polls. Every send is recorded in the ledger as `Pending`
//! before the POST, so a replica fenced out by a newer leader can never post,
//! and a send whose outcome is unknown is never repeated by anyone.
//!
//! ## Cold start
//!
//! With `cold_start_suppress = true` (default) the first poll against an
//! empty ledger only records events, no Slack messages are sent. A ledger
//! that already has a cursor resumes from it instead.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{error, info, instrument, warn};

use crate::aws::health::{EventSummary, HealthClient};
use crate::filter::EventFilter;
use crate::health::HealthEvent;
use crate::ledger::DeliveryState;
use crate::ledger::journal::{Journal, JournalError};
use crate::notify::Notifier;
use crate::observability::metrics::Metrics;
use crate::slack::error::SlackError;

const PRUNE_AFTER_HOURS: i64 = 72;
/// Re-reads this far behind the cursor for events whose `lastUpdatedTime` lands late.
const CURSOR_OVERLAP_MINUTES: i64 = 5;
/// A resumed cursor never reaches further back than this.
const MAX_RESUME_HOURS: i64 = 24;
const MAX_SEND_ATTEMPTS: u32 = 3;

pub struct PollerCfg {
    pub interval: Duration,
    pub initial_lookback: Duration,
    pub cold_start_suppress: bool,
    pub services: Vec<String>,
    pub categories: Vec<String>,
    /// Reminder offsets in hours before `startTime`. Empty disables.
    pub reminder_offsets_hours: Vec<u32>,
    /// First delay between attempts when Slack's outcome is unknown. Doubles per attempt.
    pub retry_backoff: Duration,
}

pub struct Poller {
    aws: HealthClient,
    notifier: Notifier,
    filter: EventFilter,
    metrics: Metrics,
    journal: Mutex<Journal>,
    identity: String,
    cfg: PollerCfg,
}

#[derive(Debug, Error)]
enum CycleError {
    #[error("ledger was claimed by another replica")]
    Fenced,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<JournalError> for CycleError {
    fn from(e: JournalError) -> Self {
        match e {
            JournalError::Fenced => Self::Fenced,
            other => Self::Other(other.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Standby,
    Active,
    /// Still holds the Lease but lost the ledger. Idle until re-elected.
    Fenced,
}

#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Sent,
    /// Not posted. Retried next cycle.
    Deferred,
    /// Not posted, and Slack asked us to slow down.
    RateLimited,
    Skipped,
    Unknown,
}

impl Poller {
    pub fn new(
        aws: HealthClient,
        notifier: Notifier,
        filter: EventFilter,
        metrics: Metrics,
        journal: Journal,
        identity: String,
        cfg: PollerCfg,
    ) -> Self {
        Self {
            aws,
            notifier,
            filter,
            metrics,
            journal: Mutex::new(journal),
            identity,
            cfg,
        }
    }

    /// Polls on every tick while `leader` is true, and immediately on promotion.
    pub async fn run(
        &self,
        mut leader: watch::Receiver<bool>,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let mut ticker = interval(self.cfg.interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut role = Role::Standby;

        info!(
            interval_secs = self.cfg.interval.as_secs(),
            initial_lookback_secs = self.cfg.initial_lookback.as_secs(),
            cold_start_suppress = self.cfg.cold_start_suppress,
            reminder_offsets_hours = ?self.cfg.reminder_offsets_hours,
            "poller started"
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                changed = leader.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                _ = shutdown.changed() => {
                    info!("poller shutting down");
                    return Ok(());
                }
            }
            let is_leader = *leader.borrow_and_update();
            role = self.step(role, is_leader).await;
        }
    }

    async fn step(&self, role: Role, is_leader: bool) -> Role {
        if !is_leader {
            if role != Role::Standby {
                tracing::debug!("poller paused");
            }
            return Role::Standby;
        }
        match role {
            Role::Fenced => return Role::Fenced,
            Role::Standby => {
                if let Err(e) = self.claim().await {
                    warn!(error = %e, "failed to claim the delivery ledger; retrying next tick");
                    return Role::Standby;
                }
            }
            Role::Active => {}
        }

        match self.cycle().await {
            Ok(()) => {
                self.metrics.record_poll("ok");
                Role::Active
            }
            Err(CycleError::Other(e)) => {
                error!(error = %e, "poll cycle failed");
                self.metrics.record_poll("error");
                Role::Active
            }
            Err(CycleError::Fenced) => {
                self.metrics.record_poll("error");
                self.metrics.record_ledger_conflict();
                match self.journal.lock().await.resync(&self.identity).await {
                    Ok(true) => Role::Active,
                    Ok(false) => {
                        warn!("delivery ledger claimed by another replica; idle until re-elected");
                        Role::Fenced
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to reload the delivery ledger");
                        Role::Active
                    }
                }
            }
        }
    }

    async fn claim(&self) -> Result<(), JournalError> {
        let (abandoned, term) = {
            let mut journal = self.journal.lock().await;
            let abandoned = journal.claim(&self.identity).await?;
            (abandoned, journal.ledger().term)
        };
        info!(
            term,
            abandoned = abandoned.len(),
            "claimed the delivery ledger"
        );
        for a in abandoned {
            self.notifier
                .report_unknown(&a.arn, a.reminder_offset_hours)
                .await;
        }
        Ok(())
    }

    async fn cycle(&self) -> Result<(), CycleError> {
        let now = Utc::now();
        let cursor = self.journal.lock().await.ledger().cursor;
        let (from, suppress) = self.window(cursor, now);

        let floor = self.poll_once(from, now, suppress).await?;
        let live_reminders = if self.cfg.reminder_offsets_hours.is_empty() {
            None
        } else {
            self.poll_reminders(suppress).await?
        };

        let next_cursor = floor.map_or(now, |f| f.min(now));
        let mut journal = self.journal.lock().await;
        journal
            .commit(|l| {
                l.cursor = Some(next_cursor);
                l.prune_events(ChronoDuration::hours(PRUNE_AFTER_HOURS), now);
                if let Some(live) = &live_reminders {
                    l.retain_reminders(|arn| live.contains(arn));
                }
            })
            .await?;
        tracing::debug!(
            ledger_size = journal.ledger().len(),
            cursor = %next_cursor,
            "ledger state after cycle"
        );
        Ok(())
    }

    /// Start of the poll window, and whether this is a cold start to suppress.
    fn window(&self, cursor: Option<DateTime<Utc>>, now: DateTime<Utc>) -> (DateTime<Utc>, bool) {
        cursor.map_or_else(
            || {
                let lookback =
                    ChronoDuration::from_std(self.cfg.initial_lookback).unwrap_or_default();
                (now - lookback, self.cfg.cold_start_suppress)
            },
            |c| {
                let from = (c - ChronoDuration::minutes(CURSOR_OVERLAP_MINUTES))
                    .max(now - ChronoDuration::hours(MAX_RESUME_HOURS));
                (from, false)
            },
        )
    }

    /// Returns the earliest `lastUpdatedTime` left for a retry, which caps the
    /// next cursor so the event is listed again.
    #[instrument(skip(self), fields(from = %from, to = %to, suppress))]
    async fn poll_once(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        suppress: bool,
    ) -> Result<Option<DateTime<Utc>>, CycleError> {
        let summaries = self
            .aws
            .list_events(from, to, &self.cfg.services, &self.cfg.categories)
            .await?;

        info!(
            fetched = summaries.len(),
            suppress, "received event summaries"
        );

        let mut floor = None;
        let mut suppressed = Vec::new();
        let mut rate_limited = false;
        for summary in summaries {
            let arn = summary.arn.clone();
            let last_updated = summary.last_updated_time;

            self.metrics.record_event(
                summary.service.as_deref().unwrap_or("unknown"),
                summary.event_type_category.as_deref().unwrap_or("unknown"),
                summary.region.as_deref().unwrap_or("unknown"),
            );

            if !self
                .journal
                .lock()
                .await
                .ledger()
                .should_process(&arn, last_updated)
            {
                continue;
            }
            if suppress {
                suppressed.push((arn, last_updated));
                continue;
            }
            if rate_limited {
                lower(&mut floor, last_updated);
                continue;
            }

            let event = match self.aws.hydrate(summary).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(arn = %arn, error = %e, "failed to hydrate event details; will retry next cycle");
                    lower(&mut floor, last_updated);
                    continue;
                }
            };

            let decision = self.filter.evaluate(&event);
            if !decision.is_allowed() {
                self.metrics.record_filtered(decision.reason());
                self.journal
                    .lock()
                    .await
                    .commit(|l| l.set_event(&arn, last_updated, DeliveryState::Skipped))
                    .await?;
                continue;
            }

            match self.deliver(&arn, last_updated, &event).await? {
                Delivery::Deferred => lower(&mut floor, last_updated),
                Delivery::RateLimited => {
                    rate_limited = true;
                    lower(&mut floor, last_updated);
                }
                Delivery::Sent | Delivery::Skipped | Delivery::Unknown => {}
            }
        }

        if !suppressed.is_empty() {
            self.journal
                .lock()
                .await
                .commit(|l| {
                    for (arn, last_updated) in &suppressed {
                        l.set_event(arn, *last_updated, DeliveryState::Skipped);
                    }
                })
                .await?;
            for _ in &suppressed {
                self.metrics.record_filtered("cold_start_suppressed");
            }
        }
        Ok(floor)
    }

    async fn deliver(
        &self,
        arn: &str,
        last_updated: Option<DateTime<Utc>>,
        event: &HealthEvent,
    ) -> Result<Delivery, CycleError> {
        let mut journal = self.journal.lock().await;
        let previous = journal.ledger().event(arn).cloned();
        journal
            .commit(|l| l.set_event(arn, last_updated, DeliveryState::Pending))
            .await?;
        drop(journal);

        let result = self.send(event, None).await;
        let mut journal = self.journal.lock().await;
        match result {
            Ok(()) => {
                journal
                    .commit(|l| l.set_event(arn, last_updated, DeliveryState::Sent))
                    .await?;
                info!(arn = %arn, "forwarded event");
                Ok(Delivery::Sent)
            }
            Err(e) if e.is_retry_safe() => {
                journal.commit(|l| l.restore_event(arn, previous)).await?;
                warn!(arn = %arn, error = %e, "slack did not post; will retry next cycle");
                Ok(if matches!(e, SlackError::RateLimited(_)) {
                    Delivery::RateLimited
                } else {
                    Delivery::Deferred
                })
            }
            Err(e @ SlackError::InvalidPayload(_)) => {
                journal
                    .commit(|l| l.set_event(arn, last_updated, DeliveryState::Skipped))
                    .await?;
                error!(arn = %arn, error = %e, "slack rejected the payload; skipping event");
                Ok(Delivery::Skipped)
            }
            Err(_) => {
                journal
                    .commit(|l| l.set_event(arn, last_updated, DeliveryState::Unknown))
                    .await?;
                drop(journal);
                self.notifier.report_unknown(arn, None).await;
                Ok(Delivery::Unknown)
            }
        }
    }

    /// Posts once, then retries only an ambiguous outcome (5xx, timeout after
    /// send) with exponential backoff, up to `MAX_SEND_ATTEMPTS`.
    async fn send(&self, event: &HealthEvent, reminder: Option<u32>) -> Result<(), SlackError> {
        let mut attempt = 1;
        loop {
            match self.notifier.publish(event, reminder).await {
                Err(SlackError::Ambiguous(msg)) if attempt < MAX_SEND_ATTEMPTS => {
                    warn!(attempt, error = %msg, "slack delivery outcome unknown; retrying");
                    sleep(self.cfg.retry_backoff * 2u32.pow(attempt - 1)).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Re-fetch upcoming events and fire one reminder per `(arn, offset_hours)`
    /// pair as soon as `startTime - now <= offset`. Returns the ARNs still in
    /// the upcoming window.
    async fn poll_reminders(&self, suppress: bool) -> Result<Option<HashSet<String>>, CycleError> {
        let max_offset = self
            .cfg
            .reminder_offsets_hours
            .iter()
            .max()
            .copied()
            .unwrap_or(0);
        if max_offset == 0 {
            return Ok(None);
        }
        let horizon = ChronoDuration::hours(i64::from(max_offset));
        let summaries = self
            .aws
            .list_upcoming(horizon, &self.cfg.services, &self.cfg.categories)
            .await?;

        info!(
            fetched = summaries.len(),
            max_offset_hours = max_offset,
            "received upcoming-event summaries for reminder evaluation"
        );

        let now = Utc::now();
        let live_arns: HashSet<String> = summaries.iter().map(|s| s.arn.clone()).collect();
        for summary in summaries {
            self.evaluate_reminder(summary, now, suppress).await?;
        }
        Ok(Some(live_arns))
    }

    async fn evaluate_reminder(
        &self,
        summary: EventSummary,
        now: DateTime<Utc>,
        suppress: bool,
    ) -> Result<(), CycleError> {
        let Some(start) = summary.start_time else {
            return Ok(());
        };
        // Only remind for events with a bounded maintenance window — i.e.,
        // `endTime` set. Open-ended issues without endTime fall through.
        if summary.end_time.is_none() {
            return Ok(());
        }
        let until_start = start.signed_duration_since(now);
        if until_start <= ChronoDuration::zero() {
            return Ok(());
        }
        let arn = summary.arn.clone();

        for &offset_h in &self.cfg.reminder_offsets_hours {
            if until_start > ChronoDuration::hours(i64::from(offset_h)) {
                continue;
            }
            if !self
                .journal
                .lock()
                .await
                .ledger()
                .should_remind(&arn, offset_h)
            {
                continue;
            }
            if suppress || !self.filter.evaluate(&minimal_event(&summary)).is_allowed() {
                self.journal
                    .lock()
                    .await
                    .commit(|l| l.set_reminder(&arn, offset_h, DeliveryState::Skipped))
                    .await?;
                continue;
            }
            let event = match self.aws.hydrate(summary.clone()).await {
                Ok(e) => e,
                Err(e) => {
                    warn!(arn = %arn, error = %e, "reminder hydrate failed");
                    return Ok(());
                }
            };
            self.remind(&arn, offset_h, &event).await?;
        }
        Ok(())
    }

    async fn remind(
        &self,
        arn: &str,
        offset_h: u32,
        event: &HealthEvent,
    ) -> Result<Delivery, CycleError> {
        self.journal
            .lock()
            .await
            .commit(|l| l.set_reminder(arn, offset_h, DeliveryState::Pending))
            .await?;

        let result = self.send(event, Some(offset_h)).await;
        let mut journal = self.journal.lock().await;
        match result {
            Ok(()) => {
                journal
                    .commit(|l| l.set_reminder(arn, offset_h, DeliveryState::Sent))
                    .await?;
                self.metrics.record_reminder(offset_h);
                info!(arn = %arn, offset_hours = offset_h, "fired reminder");
                Ok(Delivery::Sent)
            }
            Err(e) if e.is_retry_safe() => {
                journal.commit(|l| l.clear_reminder(arn, offset_h)).await?;
                warn!(arn = %arn, offset_hours = offset_h, error = %e, "slack did not post reminder; will retry next cycle");
                Ok(Delivery::Deferred)
            }
            Err(e @ SlackError::InvalidPayload(_)) => {
                journal
                    .commit(|l| l.set_reminder(arn, offset_h, DeliveryState::Skipped))
                    .await?;
                error!(arn = %arn, offset_hours = offset_h, error = %e, "slack rejected the reminder payload; skipping");
                Ok(Delivery::Skipped)
            }
            Err(_) => {
                journal
                    .commit(|l| l.set_reminder(arn, offset_h, DeliveryState::Unknown))
                    .await?;
                drop(journal);
                self.notifier.report_unknown(arn, Some(offset_h)).await;
                Ok(Delivery::Unknown)
            }
        }
    }
}

fn lower(floor: &mut Option<DateTime<Utc>>, last_updated: Option<DateTime<Utc>>) {
    if let Some(t) = last_updated {
        *floor = Some(floor.map_or(t, |f| f.min(t)));
    }
}

/// Build a minimal `HealthEvent` from a summary for filter evaluation only —
/// avoids a second API call before we know the event passes the filter.
fn minimal_event(s: &EventSummary) -> HealthEvent {
    HealthEvent {
        account: None,
        region: s.region.clone(),
        detail: crate::health::HealthDetail {
            event_arn: Some(s.arn.clone()),
            service: s.service.clone(),
            event_type_code: s.event_type_code.clone(),
            event_type_category: s.event_type_category.clone(),
            start_time: None,
            end_time: None,
            last_updated_time: None,
            status_code: s.status_code.clone(),
            event_description: vec![],
            affected_entities: vec![],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FilterLists;

    fn summary() -> EventSummary {
        EventSummary {
            arn: "arn:1".into(),
            service: Some("EC2".into()),
            event_type_code: Some("CODE".into()),
            event_type_category: Some("issue".into()),
            region: Some("us-east-1".into()),
            start_time: None,
            end_time: None,
            last_updated_time: None,
            status_code: Some("open".into()),
        }
    }

    #[test]
    fn minimal_event_copies_summary_fields() {
        let e = minimal_event(&summary());
        assert_eq!(e.region.as_deref(), Some("us-east-1"));
        assert_eq!(e.detail.event_arn.as_deref(), Some("arn:1"));
        assert_eq!(e.detail.service.as_deref(), Some("EC2"));
        assert_eq!(e.detail.event_type_category.as_deref(), Some("issue"));
        assert_eq!(e.detail.status_code.as_deref(), Some("open"));
        assert!(e.detail.affected_entities.is_empty());
        assert!(e.detail.event_description.is_empty());
        assert!(e.detail.start_time.is_none());
    }

    #[test]
    fn minimal_event_is_filterable() {
        let f = EventFilter::new(&FilterLists {
            allow_services: &["EC2".into()],
            ..Default::default()
        });
        assert!(f.evaluate(&minimal_event(&summary())).is_allowed());
    }

    #[test]
    fn minimal_event_carries_event_code_for_filtering() {
        let f = EventFilter::new(&FilterLists {
            deny_event_codes: &["EC2/CODE".into()],
            ..Default::default()
        });
        assert!(!f.evaluate(&minimal_event(&summary())).is_allowed());
    }

    #[test]
    fn minimal_event_carries_region_for_filtering() {
        // The reminder path filters on the summary, so a region rule must apply
        // there too rather than only on hydrated events.
        let f = EventFilter::new(&FilterLists {
            allow_regions: &["ap-northeast-2".into()],
            ..Default::default()
        });
        assert!(!f.evaluate(&minimal_event(&summary())).is_allowed());
    }

    mod cycle {
        use std::time::Duration as StdDuration;

        use secrecy::SecretString;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::super::*;
        use crate::aws::health::test_support::replay_client;
        use crate::filter::FilterLists;
        use crate::k8s::fake::FakeApiServer;
        use crate::k8s::ledger_store::LedgerStore;
        use crate::ledger::Ledger;
        use crate::notify::{Notifier, SlackOpts};
        use crate::slack::client::SlackClient;

        const LIST_ONE: &str = r#"{"events":[{"arn":"arn:1","service":"EC2","eventTypeCode":"AWS_EC2_X","eventTypeCategory":"issue","region":"us-east-1","startTime":1.7e9,"endTime":1.71e9,"lastUpdatedTime":1.72e9,"statusCode":"open"}]}"#;
        const LIST_EMPTY: &str = r#"{"events":[]}"#;
        const DETAILS: &str = r#"{"successfulSet":[{"event":{"arn":"arn:1","service":"EC2","eventTypeCode":"AWS_EC2_X","eventTypeCategory":"issue","region":"us-east-1","statusCode":"open"},"eventDescription":{"latestDescription":"hello"}}],"failedSet":[]}"#;
        const ENTITIES: &str = r#"{"entities":[{"entityValue":"i-1","statusCode":"IMPAIRED"}]}"#;

        fn hydrated_one() -> Vec<String> {
            vec![LIST_ONE.into(), DETAILS.into(), ENTITIES.into()]
        }

        async fn slack(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        fn notifier(slack_url: &str, metrics: &Metrics) -> Notifier {
            let slack = SlackClient::new(
                &SecretString::from(slack_url.to_string()),
                StdDuration::from_secs(5),
            )
            .unwrap();
            Notifier::new(
                slack,
                SlackOpts {
                    channel: None,
                    username: "bot".into(),
                    icon_emoji: ":cloud:".into(),
                    account_label: None,
                },
                None,
                metrics.clone(),
            )
        }

        fn build(
            bodies: Vec<String>,
            slack_url: &str,
            filter: EventFilter,
            journal: Journal,
            cfg: PollerCfg,
        ) -> (Poller, Metrics) {
            let (client, _h) = replay_client(bodies);
            let metrics = Metrics::new();
            let p = Poller::new(
                HealthClient::from_client(client, "en"),
                notifier(slack_url, &metrics),
                filter,
                metrics.clone(),
                journal,
                "pod-a".into(),
                cfg,
            );
            (p, metrics)
        }

        fn poller(bodies: Vec<String>, slack_url: &str, cfg: PollerCfg) -> Poller {
            build(
                bodies,
                slack_url,
                EventFilter::default(),
                Journal::memory(),
                cfg,
            )
            .0
        }

        fn cfg(reminder_offsets_hours: Vec<u32>) -> PollerCfg {
            PollerCfg {
                interval: StdDuration::from_mins(1),
                initial_lookback: StdDuration::from_hours(1),
                cold_start_suppress: false,
                services: vec![],
                categories: vec![],
                reminder_offsets_hours,
                retry_backoff: StdDuration::from_millis(1),
            }
        }

        fn hour_ago() -> DateTime<Utc> {
            Utc::now() - ChronoDuration::hours(1)
        }

        async fn state(p: &Poller, arn: &str) -> Option<DeliveryState> {
            p.journal.lock().await.ledger().event(arn).map(|e| e.state)
        }

        #[tokio::test]
        async fn poll_once_hydrates_and_publishes() {
            let server = slack(200, "ok").await;
            let p = poller(hydrated_one(), &server.uri(), cfg(vec![]));
            let floor = p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert!(floor.is_none());
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Sent));
        }

        #[tokio::test]
        async fn poll_once_suppress_only_records() {
            // No slack server needed: suppress must not publish.
            let p = poller(vec![LIST_ONE.into()], "http://127.0.0.1:1", cfg(vec![]));
            p.poll_once(hour_ago(), Utc::now(), true).await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Skipped));
        }

        #[tokio::test]
        async fn poll_once_filtered_event_is_skipped_not_published() {
            // Deny the EC2 service so the hydrated event is dropped before publish.
            let filter = EventFilter::new(&FilterLists {
                deny_services: &["EC2".into()],
                ..Default::default()
            });
            let (p, _) = build(
                hydrated_one(),
                "http://127.0.0.1:1",
                filter,
                Journal::memory(),
                cfg(vec![]),
            );
            p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Skipped));
        }

        #[tokio::test]
        async fn poll_once_skips_already_sent() {
            let p = poller(vec![LIST_ONE.into()], "http://127.0.0.1:1", cfg(vec![]));
            let t = DateTime::from_timestamp(1_720_000_000, 0);
            p.journal
                .lock()
                .await
                .commit(|l| l.set_event("arn:1", t, DeliveryState::Sent))
                .await
                .unwrap();
            assert!(
                p.poll_once(hour_ago(), Utc::now(), false)
                    .await
                    .unwrap()
                    .is_none()
            );
        }

        #[tokio::test]
        async fn not_posted_is_rolled_back_and_holds_cursor() {
            let server = slack(403, "invalid_token").await;
            let p = poller(hydrated_one(), &server.uri(), cfg(vec![]));
            let floor = p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert_eq!(floor, DateTime::from_timestamp(1_720_000_000, 0));
            assert_eq!(state(&p, "arn:1").await, None);
        }

        #[tokio::test]
        async fn rate_limited_is_rolled_back() {
            let server = slack(429, "rate_limited").await;
            let p = poller(hydrated_one(), &server.uri(), cfg(vec![]));
            assert!(
                p.poll_once(hour_ago(), Utc::now(), false)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(state(&p, "arn:1").await, None);
        }

        #[tokio::test]
        async fn invalid_payload_is_skipped() {
            let server = slack(400, "invalid_payload").await;
            let p = poller(hydrated_one(), &server.uri(), cfg(vec![]));
            assert!(
                p.poll_once(hour_ago(), Utc::now(), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Skipped));
        }

        #[tokio::test]
        async fn ambiguous_retries_then_marks_unknown() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500))
                .expect(u64::from(MAX_SEND_ATTEMPTS))
                .mount(&server)
                .await;
            let (p, metrics) = build(
                hydrated_one(),
                &server.uri(),
                EventFilter::default(),
                Journal::memory(),
                cfg(vec![]),
            );
            p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Unknown));
            assert!(
                metrics
                    .render()
                    .contains("aws_health_event_delivery_unknown_total 1")
            );
        }

        #[tokio::test]
        async fn ambiguous_then_success_is_sent() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(503))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            let p = poller(hydrated_one(), &server.uri(), cfg(vec![]));
            p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Sent));
        }

        #[tokio::test]
        async fn hydrate_failure_holds_cursor() {
            let p = poller(vec![LIST_ONE.into()], "http://127.0.0.1:1", cfg(vec![]));
            let floor = p.poll_once(hour_ago(), Utc::now(), false).await.unwrap();
            assert!(floor.is_some());
            assert_eq!(state(&p, "arn:1").await, None);
        }

        #[tokio::test]
        async fn cold_start_then_resume_from_cursor() {
            let server = slack(200, "ok").await;
            let mut c = cfg(vec![]);
            c.cold_start_suppress = true;
            let recent = (Utc::now() - ChronoDuration::minutes(10)).timestamp();
            let list = LIST_ONE.replace("1.72e9", &format!("{recent}.0"));
            let bodies = vec![list.clone(), list, DETAILS.into(), ENTITIES.into()];
            let p = poller(bodies, &server.uri(), c);

            p.cycle().await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Skipped));
            let cursor = p.journal.lock().await.ledger().cursor;
            assert!(cursor.is_some());

            // A newer lastUpdatedTime after resume is sent, not suppressed.
            p.journal
                .lock()
                .await
                .commit(|l| {
                    l.set_event(
                        "arn:1",
                        DateTime::from_timestamp(1_000, 0),
                        DeliveryState::Sent,
                    );
                })
                .await
                .unwrap();
            p.cycle().await.unwrap();
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Sent));
        }

        #[test]
        fn window_resumes_with_overlap_and_cap() {
            let p = poller(vec![], "http://127.0.0.1:1", cfg(vec![]));
            let now = Utc::now();
            let (from, suppress) = p.window(None, now);
            assert_eq!(from, now - ChronoDuration::hours(1));
            assert!(!suppress);

            let cursor = now - ChronoDuration::minutes(10);
            let (from, suppress) = p.window(Some(cursor), now);
            assert_eq!(
                from,
                cursor - ChronoDuration::minutes(CURSOR_OVERLAP_MINUTES)
            );
            assert!(!suppress);

            let (from, _) = p.window(Some(now - ChronoDuration::days(5)), now);
            assert_eq!(from, now - ChronoDuration::hours(MAX_RESUME_HOURS));
        }

        fn upcoming_list() -> String {
            let start = (Utc::now() + ChronoDuration::hours(1)).timestamp();
            let end = start + 3600;
            format!(
                r#"{{"events":[{{"arn":"arn:r","service":"EC2","eventTypeCode":"AWS_EC2_X","eventTypeCategory":"scheduledChange","region":"us-east-1","startTime":{start}.0,"endTime":{end}.0,"lastUpdatedTime":{start}.0,"statusCode":"upcoming"}}]}}"#
            )
        }

        async fn should_remind(p: &Poller) -> bool {
            p.journal.lock().await.ledger().should_remind("arn:r", 24)
        }

        #[tokio::test]
        async fn poll_reminders_fires_for_upcoming_event() {
            let server = slack(200, "ok").await;
            let p = poller(
                vec![upcoming_list(), DETAILS.into(), ENTITIES.into()],
                &server.uri(),
                cfg(vec![24]),
            );
            let live = p.poll_reminders(false).await.unwrap().unwrap();
            assert!(live.contains("arn:r"));
            assert!(!should_remind(&p).await);
        }

        #[tokio::test]
        async fn reminder_outcomes_follow_slack_result() {
            for (status, body, expect) in [
                (403, "no_service", None),
                (400, "invalid_payload", Some(DeliveryState::Skipped)),
                (500, "rollup_error", Some(DeliveryState::Unknown)),
            ] {
                let server = slack(status, body).await;
                let mut bodies = vec![upcoming_list(), DETAILS.into(), ENTITIES.into()];
                bodies.extend([DETAILS.into(), ENTITIES.into()]);
                let p = poller(bodies, &server.uri(), cfg(vec![24]));
                p.poll_reminders(false).await.unwrap();
                let got = p
                    .journal
                    .lock()
                    .await
                    .ledger()
                    .reminders
                    .get("arn:r")
                    .and_then(|o| o.get(&24).copied());
                assert_eq!(got, expect, "status {status}");
            }
        }

        #[tokio::test]
        async fn poll_reminders_suppress_marks_without_publish() {
            let p = poller(vec![upcoming_list()], "http://127.0.0.1:1", cfg(vec![24]));
            p.poll_reminders(true).await.unwrap();
            assert!(!should_remind(&p).await);
        }

        #[tokio::test]
        async fn poll_reminders_skips_event_without_end_time() {
            let start = (Utc::now() + ChronoDuration::hours(1)).timestamp();
            let list = format!(
                r#"{{"events":[{{"arn":"arn:r","service":"EC2","eventTypeCode":"AWS_EC2_X","eventTypeCategory":"scheduledChange","region":"us-east-1","startTime":{start}.0,"lastUpdatedTime":{start}.0,"statusCode":"upcoming"}}]}}"#
            );
            let p = poller(vec![list], "http://127.0.0.1:1", cfg(vec![24]));
            p.poll_reminders(false).await.unwrap();
            // No end_time → never fires; record absent.
            assert!(should_remind(&p).await);
        }

        #[tokio::test]
        async fn poll_reminders_empty_offsets_is_noop() {
            let p = poller(vec![], "http://127.0.0.1:1", cfg(vec![]));
            assert!(p.poll_reminders(false).await.unwrap().is_none());
        }

        fn configmap_journal(server: &FakeApiServer) -> Journal {
            Journal::configmap(LedgerStore::new(server.client(), "ns", "ledger"))
        }

        #[tokio::test]
        async fn step_claims_cycles_and_yields_to_new_leader() {
            let api = FakeApiServer::start().await;
            let (p, metrics) = build(
                vec![LIST_EMPTY.into(), LIST_EMPTY.into()],
                "http://127.0.0.1:1",
                EventFilter::default(),
                configmap_journal(&api),
                cfg(vec![]),
            );
            assert_eq!(p.step(Role::Standby, false).await, Role::Standby);
            assert_eq!(p.step(Role::Standby, true).await, Role::Active);
            assert_eq!(p.journal.lock().await.ledger().term, 1);

            let mut other = configmap_journal(&api);
            other.claim("pod-b").await.unwrap();

            assert_eq!(p.step(Role::Active, true).await, Role::Fenced);
            assert!(
                metrics
                    .render()
                    .contains("aws_health_event_ledger_conflicts_total 1")
            );
            assert_eq!(p.step(Role::Fenced, true).await, Role::Fenced);
            assert_eq!(p.step(Role::Fenced, false).await, Role::Standby);
        }

        #[tokio::test]
        async fn takeover_reports_abandoned_sends() {
            let api = FakeApiServer::start().await;
            let mut old = configmap_journal(&api);
            old.claim("pod-old").await.unwrap();
            old.commit(|l| {
                l.set_event("arn:1", Some(Utc::now()), DeliveryState::Pending);
            })
            .await
            .unwrap();

            let (p, metrics) = build(
                vec![LIST_EMPTY.into()],
                "http://127.0.0.1:1",
                EventFilter::default(),
                configmap_journal(&api),
                cfg(vec![]),
            );
            assert_eq!(p.step(Role::Standby, true).await, Role::Active);
            assert_eq!(state(&p, "arn:1").await, Some(DeliveryState::Unknown));
            assert!(
                metrics
                    .render()
                    .contains("aws_health_event_delivery_unknown_total 1")
            );
        }

        #[tokio::test]
        async fn claim_failure_stays_standby() {
            let api = FakeApiServer::start().await;
            api.fail_all();
            let (p, _) = build(
                vec![],
                "http://127.0.0.1:1",
                EventFilter::default(),
                configmap_journal(&api),
                cfg(vec![]),
            );
            assert_eq!(p.step(Role::Standby, true).await, Role::Standby);
        }

        #[tokio::test]
        async fn aws_failure_keeps_cursor_and_role() {
            let (p, metrics) = build(
                vec![],
                "http://127.0.0.1:1",
                EventFilter::default(),
                Journal::memory(),
                cfg(vec![]),
            );
            assert_eq!(p.step(Role::Standby, true).await, Role::Active);
            assert!(p.journal.lock().await.ledger().cursor.is_none());
            assert!(metrics.render().contains("outcome=\"error\""));
        }

        #[tokio::test]
        async fn run_polls_only_while_leader() {
            let mut c = cfg(vec![]);
            c.interval = StdDuration::from_hours(1);
            let p = std::sync::Arc::new(poller(vec![LIST_EMPTY.into()], "http://127.0.0.1:1", c));
            let (leader_tx, leader_rx) = watch::channel(false);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let runner = p.clone();
            let handle = tokio::spawn(async move { runner.run(leader_rx, shutdown_rx).await });

            tokio::time::sleep(StdDuration::from_millis(50)).await;
            assert_eq!(p.journal.lock().await.ledger(), &Ledger::default());

            leader_tx.send(true).unwrap();
            for _ in 0..100 {
                if p.journal.lock().await.ledger().cursor.is_some() {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
            assert!(p.journal.lock().await.ledger().cursor.is_some());

            shutdown_tx.send(true).unwrap();
            handle.await.unwrap().unwrap();
        }
    }
}
