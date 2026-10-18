use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use forklift::{coverage, meta, metrics, notify, repo};
use tokio_util::sync::CancellationToken;

use crate::LOG_TARGET;
use crate::bridge::block_on;

/// Clean/Dirty breakdown for grouped alarms: returns a coordinate's stored max
/// severity ("none" = Clean), the top CVSS score at that severity, and whether
/// it was scanned. Unscanned coordinates count as Dirty with no CVE.
pub(crate) fn clean_checker(store: &Arc<meta::Store>) -> notify::CleanChecker {
    let store = Arc::clone(store);
    Arc::new(move |repo_id, pkg, version| {
        let store = Arc::clone(&store);
        let (pkg, version) = (pkg.to_string(), version.to_string());
        let empty = || (String::new(), String::new(), String::new(), false);
        block_on(async move {
            let query = async {
                let repository = store.get_repository(repo_id).await.ok()?;
                let eco = repo::osv_ecosystem(&repository.format);
                if eco.is_empty() {
                    return None;
                }
                // Unscanned coordinates have no row.
                let scan = store.get_vuln_scan(eco, &pkg, &version).await.ok()?;
                // Highest-scoring advisory at the max severity carries the
                // score and id.
                let (mut score, mut id, mut best) = (String::new(), String::new(), 0.0f64);
                for a in &scan.advisories {
                    if a.severity != scan.max_severity {
                        continue;
                    }
                    let f = a.score.parse::<f64>().unwrap_or(0.0);
                    if id.is_empty() || f >= best {
                        best = f;
                        score.clone_from(&a.score);
                        id.clone_from(&a.id);
                    }
                }
                Some((scan.max_severity, score, id, true))
            };
            match tokio::time::timeout(Duration::from_secs(3), query).await {
                Ok(Some(found)) => found,
                _ => empty(),
            }
        })
    })
}

/// Sends the approval alarm to the receivers a repository selected, resolved
/// against the enabled receivers managed in the admin console, and records
/// the alarm targets on the approval row. Runs off the serving path.
pub(crate) fn approval_notifier(
    store: &Arc<meta::Store>,
    notifier: &Arc<notify::Notifier>,
) -> impl Fn(String, i64, String, String, String, String, Vec<String>) + Send + Sync + 'static {
    let store = Arc::clone(store);
    let notifier = Arc::clone(notifier);
    move |repo_name: String,
          repo_id: i64,
          repo_format: String,
          pkg: String,
          version: String,
          requested_by: String,
          receivers: Vec<String>| {
        if receivers.is_empty() {
            return;
        }
        let store = Arc::clone(&store);
        let notifier = Arc::clone(&notifier);
        tokio::spawn(async move {
            let work = async {
                let all = match store.list_enabled_receivers().await {
                    Ok(all) => all,
                    Err(e) => {
                        tracing::warn!(target: LOG_TARGET, err = %e, "notify: list receivers failed");
                        return;
                    }
                };
                let selected: std::collections::HashSet<&str> =
                    receivers.iter().map(String::as_str).collect();
                let mut targets = Vec::new();
                let mut names = Vec::new();
                for rec in &all {
                    if selected.contains(rec.name.as_str()) {
                        targets.push(notify::Target {
                            name: rec.name.clone(),
                            url: rec.webhook_url.clone(),
                        });
                        names.push(rec.name.clone());
                    }
                }
                notifier.notify_approval_request(
                    &targets,
                    &repo_name,
                    repo_id,
                    &repo_format,
                    &pkg,
                    &version,
                    &requested_by,
                );
                // Record the alarm targets on the approval row for the
                // queue's column.
                if !names.is_empty()
                    && let Err(e) = store.mark_approval_notified(&repo_name, &pkg, &names).await
                {
                    tracing::warn!(target: LOG_TARGET, err = %e, "notify: mark approval notified failed");
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
        });
    }
}

/// Persists each approval alarm's actual delivery outcome (send time, result and
/// elapsed time) onto the covered approval rows, for the review detail page.
/// Invoked from the (async, possibly batched) delivery task.
pub(crate) fn delivery_recorder(store: &Arc<meta::Store>) -> notify::DeliveryRecorder {
    let store = Arc::clone(store);
    Arc::new(move |pkgs, result, detail, duration_ms| {
        let store = Arc::clone(&store);
        let (result, detail) = (result.to_string(), detail.to_string());
        block_on(async move {
            let work = async {
                for p in &pkgs {
                    if let Err(e) = store
                        .record_approval_delivery(
                            &p.repo,
                            &p.package,
                            &result,
                            &detail,
                            duration_ms,
                        )
                        .await
                    {
                        tracing::warn!(target: LOG_TARGET,
                            repo = %p.repo,
                            package = %p.package,
                            err = %e,
                            "notify: record approval delivery failed"
                        );
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
        });
    })
}

/// Pending approvals, computed on scrape (one indexed COUNT).
pub(crate) fn approval_pending(store: &Arc<meta::Store>) -> Arc<dyn Fn() -> f64 + Send + Sync> {
    let store = Arc::clone(store);
    Arc::new(move || {
        let store = Arc::clone(&store);
        block_on(async move {
            let count = store.count_approvals("", meta::APPROVAL_PENDING);
            match tokio::time::timeout(Duration::from_secs(2), count).await {
                Ok(Ok(n)) => n as f64,
                _ => 0.0,
            }
        })
    })
}

/// Reads the coverage scanner's in-memory picture for the metrics collector.
pub(crate) fn coverage_stats(
    scanner: &Arc<coverage::Scanner>,
) -> Arc<dyn Fn() -> metrics::CoverageStats + Send + Sync> {
    let scanner = Arc::clone(scanner);
    Arc::new(move || {
        let overview = scanner.overview();
        let stats = scanner.scan_stats();
        metrics::CoverageStats {
            enabled: overview.enabled,
            target: overview.summary.target,
            applied: overview.summary.applied,
            partial: overview.summary.partial,
            not_applied: overview.summary.not_applied,
            errored: overview.summary.errored,
            no_ci: overview.summary.skipped,
            muted: overview.summary.excluded,
            percent: overview.summary.percent,
            last_scanned_at: overview.last_scanned_at,
            last_scan_seconds: overview.last_scan_duration_ms as f64 / 1000.0,
            scanning: overview.scanning,
            last_scan_failed: !overview.last_scan_error.is_empty(),
            scans_succeeded: stats.succeeded,
            scans_failed: stats.failed,
            concurrency: stats.last_concurrency,
            peak_concurrency: stats.peak_concurrency,
        }
    })
}

/// Builds an on-demand trigger for a leader-only job that outlives the request
/// asking for it. It is bound to the process `shutdown` token instead, and
/// reports whether the job was accepted: refused on a standby and while
/// `busy` reports a run already in progress.
pub(crate) fn leader_trigger<T, B, R, Fut, O, E>(
    leader_state: &Arc<AtomicBool>,
    shutdown: &CancellationToken,
    target: &Arc<T>,
    busy: B,
    run: R,
    failure: &'static str,
) -> Arc<dyn Fn(&str) -> bool + Send + Sync>
where
    T: Send + Sync + 'static,
    B: Fn(&T) -> bool + Send + Sync + 'static,
    R: Fn(Arc<T>, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<O, E>> + Send + 'static,
    O: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let leader_state = Arc::clone(leader_state);
    let shutdown = shutdown.clone();
    let target = Arc::clone(target);
    Arc::new(move |triggered_by: &str| {
        if !leader_state.load(Ordering::SeqCst) || busy(&target) {
            return false;
        }
        let job = run(Arc::clone(&target), triggered_by.to_string());
        let shutdown = shutdown.clone();
        let triggered_by = triggered_by.to_string();
        tokio::spawn(async move {
            tokio::select! {
                () = shutdown.cancelled() => {}
                res = job => {
                    if let Err(e) = res {
                        tracing::warn!(target: LOG_TARGET, triggered_by = %triggered_by, err = %e, "{failure}");
                    }
                }
            }
        });
        true
    })
}
