//! Runs the rules on a schedule and stores each report.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use tokio_util::sync::CancellationToken;

use crate::meta::{self, InsightReportRow, Store};
use crate::notify;

use super::collect::{DataSnapshot, collect};
use super::model::{InsightSummary, Inventory, Report, ScanCount};
use super::rules::evaluate;
use super::settings::{self, Settings};

/// How long trend rows are kept.
const HISTORY: TimeDelta = TimeDelta::days(90);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] meta::Error),
    #[error("encode insight report: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("an analysis is already running")]
    Busy,
}

/// Owns the analysis. Only the leader runs it, because a run writes the report.
pub struct Analyzer {
    store: Arc<Store>,
    coverage: Option<Arc<crate::coverage::Scanner>>,
    /// The analysis interval until somebody saves settings
    /// (FORKLIFT_INSIGHTS_INTERVAL).
    default_interval: Duration,
    running: AtomicBool,
}

/// Clears the running flag however the run ends.
struct RunGuard<'a>(&'a AtomicBool);

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Analyzer {
    /// `coverage`, when given and enabled, adds the projects that bypass
    /// forklift to the analysis.
    pub fn new(
        store: Arc<Store>,
        coverage: Option<Arc<crate::coverage::Scanner>>,
        default_interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            coverage,
            default_interval,
            running: AtomicBool::new(false),
        })
    }

    /// Returns the saved settings, or the defaults before the first save.
    pub async fn settings(&self) -> Result<Settings, Error> {
        Ok(self.store.read_insight_settings().await?.map_or_else(
            || Settings::defaults(self.default_interval),
            Settings::from_row,
        ))
    }

    pub async fn save_settings(&self, settings: &Settings) -> Result<(), Error> {
        Ok(self.store.write_insight_settings(settings.to_row()).await?)
    }

    /// When the next scheduled analysis is due, `None` while it is off.
    pub async fn next_analysis_at(
        &self,
        settings: &Settings,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, Error> {
        if !settings.auto_analyze_enabled {
            return Ok(None);
        }
        Ok(Some(
            self.store
                .latest_insight_generated_at()
                .await?
                .map_or(now, |last| (last + settings.interval()).max(now)),
        ))
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Analyzes once and stores the report. Refuses with [`Error::Busy`] while
    /// another run is in flight.
    pub async fn run_once(&self, triggered_by: &str) -> Result<Report, Error> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(Error::Busy);
        }
        let _guard = RunGuard(&self.running);
        let started = std::time::Instant::now();
        let now = Utc::now();
        let snapshot = collect(&self.store, self.coverage.as_deref(), now).await?;
        let (findings, checks) = evaluate(&snapshot);
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let report = Report::new(
            now,
            duration_ms,
            triggered_by,
            inventory(&snapshot),
            findings,
            checks,
        );
        // The history records each check as people saw it at the time, with
        // the acknowledgements then in force.
        let mut seen = report.clone();
        seen.apply_acknowledgements(&self.store.list_insight_acks().await?, now);
        let points = check_points(&seen);
        self.store
            .insert_insight_report(
                InsightReportRow {
                    generated_at: report.generated_at,
                    score: report.score,
                    summary_json: serde_json::to_string(&report.summary())?,
                    report_json: serde_json::to_string(&report)?,
                },
                points,
                now - HISTORY,
            )
            .await?;
        tracing::info!(
            score = report.score,
            findings = report.findings.len(),
            duration_ms,
            triggered_by,
            "security insights updated"
        );
        Ok(report)
    }

    /// Runs the schedule until cancelled: the analysis whenever the interval has
    /// passed since the last report, and the alarm whenever its cron fires.
    /// Both read the settings on every tick, so a save takes effect within a
    /// minute. The last report's time is read from the store, so a new leader
    /// keeps the cadence.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken, notifier: Arc<notify::Notifier>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Keeps the alarm from going out twice when two ticks share a minute.
        let mut last_fired: Option<i64> = None;
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let now = Utc::now();
            let settings = match self.settings().await {
                Ok(settings) => settings,
                Err(err) => {
                    tracing::warn!(err = %err, "security insights: reading settings failed");
                    continue;
                }
            };
            match self.next_analysis_at(&settings, now).await {
                Ok(Some(due)) if due <= now => {
                    if let Err(err) = self.run_once("schedule").await {
                        tracing::error!(err = %err, "security insights failed");
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(err = %err, "security insights: reading the last run failed");
                }
            }
            if !settings.report_enabled || settings.receiver.is_empty() {
                continue;
            }
            let schedule = match crate::coverage::parse_schedule(
                &settings.report_cron,
                &settings.timezone,
            ) {
                Ok(schedule) => schedule,
                Err(err) => {
                    tracing::warn!(cron = %settings.report_cron, err = %err, "security insights: alarm schedule is not usable");
                    continue;
                }
            };
            let minute = now.timestamp().div_euclid(60);
            if !schedule.matches(now) || last_fired == Some(minute) {
                continue;
            }
            last_fired = Some(minute);
            if let Err(err) = self.send_scheduled_report(&settings, &notifier, now).await {
                tracing::warn!(err = %err, "security insights: scheduled alarm failed");
            }
        }
    }

    /// Sends the scheduled alarm for the newest report, unless no open finding
    /// reaches the minimum severity or, with skip_when_unchanged, nothing has
    /// turned up since the last one. Reports whether it was sent.
    pub async fn send_scheduled_report(
        &self,
        settings: &Settings,
        notifier: &notify::Notifier,
        now: DateTime<Utc>,
    ) -> Result<bool, Error> {
        let Some(mut report) = self.latest().await? else {
            tracing::info!("security insights: alarm skipped, nothing analyzed yet");
            return Ok(false);
        };
        report.apply_acknowledgements(&self.store.list_insight_acks().await?, now);
        let open = settings::open_at_or_above(&report, settings.min_severity);
        if open.is_empty() {
            tracing::info!(
                min_severity = settings.min_severity.label(),
                "security insights: alarm skipped, nothing at or above the minimum severity"
            );
            return Ok(false);
        }
        if settings.skip_when_unchanged
            && !settings::has_news(&self.store.insight_report_state().await?, &open)
        {
            tracing::info!("security insights: alarm skipped, nothing new since the last one");
            return Ok(false);
        }
        let receiver = self
            .store
            .list_enabled_receivers()
            .await?
            .into_iter()
            .find(|r| r.name == settings.receiver && !r.webhook_url.is_empty());
        let Some(receiver) = receiver else {
            tracing::warn!(receiver = %settings.receiver, "security insights: the alarm receiver is missing or disabled");
            return Ok(false);
        };
        let alarm = super::alarm_report(&report);
        let payload = notifier.build_insights_report(&alarm);
        let sent = notifier
            .send_insights_report(&receiver.webhook_url, &payload)
            .await;
        let error = sent.as_ref().err().map(notify::Error::public_detail);
        self.store
            .log_alarm_delivery(
                meta::ALARM_INSIGHTS,
                &receiver.name,
                meta::ALARM_TRIGGER_SCHEDULE,
                error.as_deref().map_or(Ok(()), Err),
                alarm.findings.len() as i64,
            )
            .await;
        if let Err(err) = sent {
            tracing::warn!(receiver = %receiver.name, err = %err, "security insights: alarm delivery failed");
            return Ok(false);
        }
        self.store.write_insight_report_state(&open).await?;
        tracing::info!(receiver = %receiver.name, findings = open.len(), "security insights: scheduled alarm sent");
        Ok(true)
    }

    /// Returns the newest stored report, `None` before the first run.
    pub async fn latest(&self) -> Result<Option<Report>, Error> {
        match self.store.latest_insight_report().await {
            Ok(row) if !row.report_json.is_empty() => {
                let mut report: Report = serde_json::from_str(&row.report_json)?;
                backfill_codes(&mut report);
                Ok(Some(report))
            }
            Ok(_) => Ok(None),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns the trend since `since`, oldest first. Rows that no longer
    /// decode are skipped.
    pub async fn history(&self, since: DateTime<Utc>) -> Result<Vec<InsightSummary>, Error> {
        Ok(self
            .store
            .insight_history(since)
            .await?
            .into_iter()
            .filter_map(|row| serde_json::from_str(&row.summary_json).ok())
            .collect())
    }
}

/// One history point per check of a report.
fn check_points(report: &Report) -> Vec<meta::InsightCheckPoint> {
    report
        .checks
        .iter()
        .map(|c| meta::InsightCheckPoint {
            check_id: c.id.clone(),
            generated_at: report.generated_at,
            status: serde_json::to_value(c.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            severity: report
                .findings
                .iter()
                .find(|f| f.id == c.id)
                .map(|f| f.severity.label().to_string())
                .unwrap_or_default(),
            flagged: c.flagged,
        })
        .collect()
}

impl Analyzer {
    /// Returns one check's history since `since`, oldest first.
    pub async fn check_history(
        &self,
        check_id: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<meta::InsightCheckPoint>, Error> {
        Ok(self.store.insight_check_history(check_id, since).await?)
    }
}

/// Gives a report stored before checks had codes the codes of its rules.
fn backfill_codes(report: &mut Report) {
    let code = |id: &str| {
        super::rules::CATALOG
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.code.to_string())
            .unwrap_or_default()
    };
    for check in report.checks.iter_mut().filter(|c| c.code.is_empty()) {
        check.code = code(&check.id);
    }
    for finding in report.findings.iter_mut().filter(|f| f.code.is_empty()) {
        finding.code = code(&finding.id);
    }
}

fn inventory(s: &DataSnapshot) -> Inventory {
    let mut scannable: HashSet<(i64, String)> = HashSet::new();
    let mut scanned = 0;
    let mut by_repo: BTreeMap<String, ScanCount> = BTreeMap::new();
    for art in &s.artifacts {
        let Some(view) = s.repo(art.repo_id) else {
            continue;
        };
        let (eco, pkg) = crate::repo::vuln_coordinate(&view.repo.format, &art.path);
        let version = crate::repo::version_for_path(&view.repo.format, &art.path);
        if pkg.is_empty() || version.is_empty() {
            continue;
        }
        let key = meta::coordinate_key(&eco, &pkg, &version);
        if scannable.insert((art.repo_id, key.clone())) {
            let count = by_repo.entry(view.repo.name.clone()).or_default();
            count.scannable += 1;
            if s.vulns.contains_key(&key) {
                count.scanned += 1;
                scanned += 1;
            }
        }
    }
    Inventory {
        repositories: s.repos.len() as i64,
        artifacts: s
            .repos
            .iter()
            .filter(|v| v.repo.format != meta::FORMAT_OCI)
            .map(|v| s.row_counts.get(&v.repo.id).copied().unwrap_or_default())
            .sum(),
        oci_images: s.oci_tags.values().map(|t| t.len() as i64).sum(),
        scanned,
        scannable: scannable.len() as i64,
        users: s.users.iter().filter(|u| !u.disabled).count() as i64,
        tokens: s.tokens.len() as i64,
        scan_by_repo: by_repo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insights::model::{Category, Check, CheckStatus};

    #[test]
    fn backfill_codes_fills_reports_stored_before_codes() {
        let mut report = Report::new(
            Utc::now(),
            1,
            "schedule",
            Inventory::default(),
            Vec::new(),
            vec![Check {
                id: "tokens-unscoped-admin".to_string(),
                code: String::new(),
                category: Category::Access,
                audiences: Vec::new(),
                title: String::new(),
                rationale: String::new(),
                criteria: String::new(),
                recommendation: String::new(),
                status: CheckStatus::Ok,
                flagged: 0,
                acknowledgement: None,
            }],
        );
        backfill_codes(&mut report);
        assert_eq!(report.checks[0].code, "AC-05");
    }
}
