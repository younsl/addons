use std::sync::Arc;
use std::time::Duration;

use crate::LOG_TARGET;
use chrono::Utc;
use forklift::{coverage, meta, notify};
use tokio_util::sync::CancellationToken;

/// Lets the rest of the process settle before the first crawl, so a cold start
/// is not competing with a GitLab walk for its first requests.
const STARTUP_SCAN_DELAY: Duration = Duration::from_secs(30);

/// Bounds one report delivery round.
const COVERAGE_DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Fires the coverage scan on the administrator's cron.
///
/// It ticks every minute and asks the schedule whether this minute is a firing one, rather than
/// sleeping until a precomputed next run.
///
/// Leader-gated by the caller: a scan writes the result, the history row and the
/// exclusion state, and SQLite has one writer.
pub(crate) async fn run_coverage_schedule(
    cancel: CancellationToken,
    store: Arc<meta::Store>,
    scanner: Arc<coverage::Scanner>,
    notifier: Arc<notify::Notifier>,
) {
    let cfg = scanner.settings();
    tracing::info!(target: LOG_TARGET,
        cron = %cfg.scan_cron,
        timezone = %cfg.timezone,
        auto = cfg.auto_scan_enabled,
        "coverage schedule started"
    );

    // A scan on the very first boot seeds the dashboard, which would otherwise
    // sit empty until the first cron firing, possibly a day later. A forklift
    // that already has a stored result does not rescan on restart: the picture is
    // there, and a rolling deploy would otherwise crawl GitLab once per pod.
    if scanner.configured() && scanner.overview().last_scanned_at.is_none() {
        tokio::spawn({
            let cancel = cancel.clone();
            let store = Arc::clone(&store);
            let scanner = Arc::clone(&scanner);
            let notifier = Arc::clone(&notifier);
            async move {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(STARTUP_SCAN_DELAY) => {}
                }
                if let Err(e) = scanner.scan(coverage::TRIGGER_STARTUP).await {
                    tracing::warn!(target: LOG_TARGET, err = %e, "coverage: initial scan failed");
                    return;
                }
                deliver_coverage_report(&store, &scanner, &notifier).await;
            }
        });
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    // last_fired keeps a scan from running twice in one minute: the tick does not
    // land on the minute boundary, so two ticks can share a minute after a drift
    // correction.
    let mut last_fired: Option<i64> = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let now = Utc::now();
        let cfg = scanner.settings();
        if !cfg.auto_scan_enabled || !scanner.configured() {
            continue;
        }
        let schedule = match coverage::parse_schedule(&cfg.scan_cron, &cfg.timezone) {
            Ok(schedule) => schedule,
            Err(e) => {
                tracing::warn!(target: LOG_TARGET,
                    cron = %cfg.scan_cron,
                    timezone = %cfg.timezone,
                    err = %e,
                    "coverage: schedule is not usable, skipping this tick"
                );
                continue;
            }
        };
        if !schedule.matches(now) {
            continue;
        }
        let minute = now.timestamp().div_euclid(60);
        if last_fired == Some(minute) {
            continue;
        }
        last_fired = Some(minute);
        if let Err(e) = scanner.scan(coverage::TRIGGER_SCHEDULE).await {
            tracing::warn!(target: LOG_TARGET, err = %e, "coverage: scheduled scan failed");
            continue;
        }
        deliver_coverage_report(&store, &scanner, &notifier).await;
    }
}

/// Posts the coverage report to the receivers the settings name.
///
/// Only the automatic scans report: the scheduled one and the first scan after a
/// cold start. A scan somebody triggered from the console does not, because
/// asking for the current number is not asking to tell the whole receiver about
/// it; that is the settings page's own send button, which sends what the last
/// completed scan measured and ignores skip_when_full_coverage because somebody
/// asked for it. A failed delivery is logged and never fails the scan that
/// produced it.
async fn deliver_coverage_report(
    store: &Arc<meta::Store>,
    scanner: &Arc<coverage::Scanner>,
    notifier: &Arc<notify::Notifier>,
) {
    let cfg = scanner.settings();
    if !cfg.report_enabled || cfg.receiver.is_empty() {
        return;
    }
    let name = cfg.receiver.clone();

    let overview = scanner.overview();
    let projects = scanner
        .not_applied()
        .into_iter()
        .map(|p| notify::CoverageProject {
            path: p.path,
            partial: p.applied == coverage::STATE_PARTIAL,
            web_url: p.web_url,
        })
        .collect();
    let report = notify::CoverageReport {
        target: overview.summary.target,
        applied: overview.summary.applied,
        partial: overview.summary.partial,
        not_applied: overview.summary.not_applied,
        errored: overview.summary.errored,
        skipped: overview.summary.skipped,
        not_applied_projects: projects,
        ..Default::default()
    };
    if cfg.skip_when_full_coverage && notify::is_full_coverage(&report) {
        tracing::info!(target: LOG_TARGET,
            applied = report.applied,
            target = report.target,
            "coverage: report skipped, coverage is full"
        );
        return;
    }

    let deliver = async {
        let all = match store.list_enabled_receivers().await {
            Ok(all) => all,
            Err(e) => {
                tracing::warn!(target: LOG_TARGET, err = %e, "coverage: listing receivers failed");
                return;
            }
        };
        let targets: Vec<notify::Target> = all
            .into_iter()
            .find(|rec| rec.name == name && !rec.webhook_url.is_empty())
            .map(|rec| {
                vec![notify::Target {
                    name: rec.name,
                    url: rec.webhook_url,
                }]
            })
            .unwrap_or_default();
        if targets.is_empty() {
            tracing::warn!(target: LOG_TARGET,
                receiver = %name,
                "coverage: the configured receiver is missing or disabled"
            );
            return;
        }
        notifier.notify_coverage_report(&targets, &report);
    };
    let _ = tokio::time::timeout(COVERAGE_DELIVER_TIMEOUT, deliver).await;
}
