//! Serves the security insight report.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{TimeDelta, Utc};
use http::StatusCode;
use http::request::Parts;
use serde::Serialize;

use crate::auth;
use crate::insights::{Category, InsightSummary, Report};
use crate::meta;
use crate::notify;

use super::notifications::{
    AlarmHistoryQuery, NotificationSampleReportDTO, NotificationSampleResultDTO,
    write_alarm_history,
};
use super::{Handler, map_error, principal_name, write_error, write_json};

/// The trend window the view draws.
const HISTORY: TimeDelta = TimeDelta::days(30);

#[derive(Debug, Serialize)]
struct InsightsDTO {
    /// The newest report narrowed to what the caller may see, absent before the
    /// first run.
    report: Option<Report>,
    /// Score and counts per run, oldest first. Only for callers who see the
    /// whole report: a narrowed score has no history to compare against.
    history: Vec<InsightSummary>,
    /// "all", or "repositories" when the report covers only the repositories
    /// the caller can read.
    scope: &'static str,
    running: bool,
    interval_seconds: u64,
    can_refresh: bool,
}

#[derive(Debug, Serialize)]
struct RefreshDTO {
    started: bool,
}

/// Who sees the whole report: administrators, auditors and security
/// reviewers. Without an authorization service (tests) everyone does.
fn sees_everything(h: &Handler, parts: &Parts) -> bool {
    h.authz.is_none()
        || auth::from_request_parts(parts)
            .is_some_and(|p| p.is_admin() || p.can_audit_any() || p.can_security_any())
}

fn may_refresh(h: &Handler, parts: &Parts) -> bool {
    h.authz.is_none()
        || auth::from_request_parts(parts).is_some_and(|p| p.is_admin() || p.can_security_any())
}

/// Keeps only what a caller limited to some repositories may see: findings
/// about repositories, cut down to the readable ones. Account, token and client
/// findings, and anything not tied to a repository, are dropped.
pub(crate) fn narrow(report: &mut Report, can_read: impl Fn(&str) -> bool) {
    report.checks.retain(|c| c.category != Category::Access);
    report.findings.retain_mut(|f| {
        if f.everyone {
            return true;
        }
        if f.category == Category::Access || f.by_repo.is_empty() {
            return false;
        }
        f.by_repo.retain(|repo, _| can_read(repo));
        f.affected
            .retain(|a| !a.repo.is_empty() && can_read(&a.repo));
        f.total = f.by_repo.values().sum();
        f.params.retain(|k, _| k == "count");
        f.params.insert("count".to_string(), f.total.to_string());
        f.summary = format!("{} affected in the repositories you can read.", f.total);
        f.total > 0
    });
    report.rescore();
}

/// The inventory as a viewer limited to some repositories sees it: their
/// repositories, what those hold and how much of it is scanned, and their own
/// access tokens. The number of users stays at zero.
async fn scoped_inventory(
    h: &Handler,
    can_read: impl Fn(&str) -> bool,
    scan_by_repo: &std::collections::BTreeMap<String, crate::insights::ScanCount>,
    username: &str,
) -> Result<crate::insights::Inventory, crate::meta::Error> {
    let repos: Vec<_> = h
        .store
        .list_repositories()
        .await?
        .into_iter()
        .filter(|r| can_read(&r.name))
        .collect();
    let stats = h.store.all_repo_stats().await?;
    let tags = h.store.oci_tag_counts().await?;
    let mut inventory = crate::insights::Inventory {
        repositories: repos.len() as i64,
        ..Default::default()
    };
    for repo in &repos {
        if repo.format == crate::meta::FORMAT_OCI {
            inventory.oci_images += tags.get(&repo.id).copied().unwrap_or_default();
        } else {
            inventory.artifacts += stats.get(&repo.id).map_or(0, |s| s.artifact_count);
        }
        if let Some(scan) = scan_by_repo.get(&repo.name) {
            inventory.scanned += scan.scanned;
            inventory.scannable += scan.scannable;
        }
    }
    inventory.tokens = match h.store.get_user_by_username(username).await {
        Ok(user) => h.store.list_tokens(user.id).await?.len() as i64,
        Err(err) if err.is_not_found() => 0,
        Err(err) => return Err(err),
    };
    Ok(inventory)
}

pub(super) async fn get(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let Some((analyzer, refresh)) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    let mut report = match analyzer.latest().await {
        Ok(report) => report,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if let Some(report) = &mut report
        && let Err(response) = acknowledge(&h, report).await
    {
        return *response;
    }
    let interval_seconds = match analyzer.settings().await {
        Ok(s) if s.auto_analyze_enabled => u64::try_from(s.interval_minutes * 60).unwrap_or(0),
        Ok(_) => 0,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let everything = sees_everything(&h, &parts);
    let history = if everything {
        match analyzer.history(Utc::now() - HISTORY).await {
            Ok(history) => history,
            Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        }
    } else {
        if let Some(report) = &mut report {
            let principal = auth::from_request_parts(&parts);
            let can_read = |repo: &str| {
                principal
                    .as_ref()
                    .is_some_and(|p| p.can(repo, auth::ACTION_READ))
            };
            narrow(report, can_read);
            let username = principal
                .as_ref()
                .map(|p| p.username.clone())
                .unwrap_or_default();
            match scoped_inventory(&h, can_read, &report.inventory.scan_by_repo, &username).await {
                Ok(inventory) => report.inventory = inventory,
                Err(err) => return map_error(err),
            }
        }
        Vec::new()
    };
    if let Some(report) = &mut report {
        report.inventory.scan_by_repo.clear();
    }
    write_json(
        StatusCode::OK,
        InsightsDTO {
            report,
            history,
            scope: if everything { "all" } else { "repositories" },
            running: analyzer.running(),
            interval_seconds,
            can_refresh: refresh.is_some() && may_refresh(&h, &parts),
        },
    )
}

pub(super) async fn refresh(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !may_refresh(&h, &parts) {
        return write_error(
            StatusCode::FORBIDDEN,
            "refreshing security insights requires admin or security permission",
        );
    }
    let Some((_, Some(start))) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    if !start(&principal_name(&parts)) {
        return write_error(
            StatusCode::CONFLICT,
            "an analysis is already running, or this instance is not the leader",
        );
    }
    h.audit(&parts, "", "insights.refresh", 200);
    write_json(StatusCode::OK, RefreshDTO { started: true })
}

#[derive(Debug, Serialize)]
struct AlarmReceiverDTO {
    id: i64,
    name: String,
    enabled: bool,
    webhook_configured: bool,
}

#[derive(Debug, Serialize)]
struct AlarmPreviewDTO {
    payload: notify::InsightsPayload,
    receivers: Vec<AlarmReceiverDTO>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AlarmSendReq {
    #[serde(default)]
    receiver_ids: Vec<i64>,
}

/// Loads what an alarm needs: the notifier and the newest full report.
async fn alarm_inputs(
    h: &Handler,
    parts: &Parts,
) -> Result<(Arc<notify::Notifier>, Report), Box<Response>> {
    if !may_refresh(h, parts) {
        return Err(Box::new(write_error(
            StatusCode::FORBIDDEN,
            "sending security insight alarms requires admin or security permission",
        )));
    }
    let Some((analyzer, _)) = h.insights() else {
        return Err(Box::new(write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        )));
    };
    let Some(notifier) = h.notifier() else {
        return Err(Box::new(write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "notifications are not configured",
        )));
    };
    match analyzer.latest().await {
        Ok(Some(mut report)) => {
            acknowledge(h, &mut report).await?;
            Ok((notifier, report))
        }
        Ok(None) => Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "there is no analysis to report yet",
        ))),
        Err(err) => Err(Box::new(write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &err.to_string(),
        ))),
    }
}

/// Renders the alarm the newest report would send and lists the receivers it
/// can go to, without sending anything.
pub(super) async fn preview_alarm(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let (notifier, report) = match alarm_inputs(&h, &parts).await {
        Ok(inputs) => inputs,
        Err(response) => return *response,
    };
    let receivers = match h.store.list_receivers().await {
        Ok(receivers) => receivers,
        Err(err) => return map_error(err),
    };
    write_json(
        StatusCode::OK,
        AlarmPreviewDTO {
            payload: notifier.build_insights_report(&crate::insights::alarm_report(&report)),
            receivers: receivers
                .into_iter()
                .map(|r| AlarmReceiverDTO {
                    id: r.id,
                    name: r.name,
                    enabled: r.enabled,
                    webhook_configured: !r.webhook_url.is_empty(),
                })
                .collect(),
        },
    )
}

/// Sends the newest report to the chosen receivers now, reporting each
/// receiver's outcome. Disabled receivers and ones without a webhook are
/// reported as failed rather than skipped silently.
pub(super) async fn send_alarm(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let (notifier, report) = match alarm_inputs(&h, &parts).await {
        Ok(inputs) => inputs,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<AlarmSendReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    if req.receiver_ids.is_empty() {
        return write_error(StatusCode::BAD_REQUEST, "select at least one receiver");
    }
    let receivers = match h.store.list_receivers().await {
        Ok(receivers) => receivers,
        Err(err) => return map_error(err),
    };
    let alarm = crate::insights::alarm_report(&report);
    let payload = notifier.build_insights_report(&alarm);
    let mut results = Vec::with_capacity(req.receiver_ids.len());
    for id in &req.receiver_ids {
        let Some(receiver) = receivers.iter().find(|r| r.id == *id) else {
            results.push(NotificationSampleResultDTO {
                name: format!("receiver {id}"),
                ok: false,
                error: "not found".to_string(),
            });
            continue;
        };
        let outcome = if !receiver.enabled {
            Err("receiver is disabled".to_string())
        } else if receiver.webhook_url.is_empty() {
            Err("receiver has no webhook".to_string())
        } else {
            notifier
                .send_insights_report(&receiver.webhook_url, &payload)
                .await
                .map_err(|err| err.public_detail())
        };
        h.store
            .log_alarm_delivery(
                meta::ALARM_INSIGHTS,
                &receiver.name,
                meta::ALARM_TRIGGER_MANUAL,
                outcome.as_ref().copied().map_err(String::as_str),
                alarm.findings.len() as i64,
            )
            .await;
        results.push(NotificationSampleResultDTO {
            name: receiver.name.clone(),
            ok: outcome.is_ok(),
            error: outcome.err().unwrap_or_default(),
        });
    }
    h.audit(&parts, "", "insights.notify", 200);
    write_json(StatusCode::OK, NotificationSampleReportDTO { results })
}

/// Lists the security alarm's recent deliveries (admin or security), so the
/// settings page can chart how the alarm went out.
pub(super) async fn alarm_history(
    State(h): State<Arc<Handler>>,
    axum::extract::Query(query): axum::extract::Query<AlarmHistoryQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if !may_refresh(&h, &parts) {
        return write_error(
            StatusCode::FORBIDDEN,
            "the alarm history requires admin or security permission",
        );
    }
    if h.insights().is_none() {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    }
    write_alarm_history(&h, meta::ALARM_INSIGHTS, &query).await
}

/// Applies the stored acknowledgements to a report as of now.
async fn acknowledge(h: &Handler, report: &mut Report) -> Result<(), Box<Response>> {
    let acks = h
        .store
        .list_insight_acks()
        .await
        .map_err(|err| Box::new(map_error(err)))?;
    report.apply_acknowledgements(&acks, Utc::now());
    Ok(())
}

/// The longest an acknowledgement may last before the check counts again.
const MAX_ACK_DAYS: i64 = 365;

#[derive(Debug, Default, serde::Deserialize)]
struct AckReq {
    #[serde(default)]
    note: String,
    #[serde(default)]
    days: Option<i64>,
}

#[derive(Debug, Serialize)]
struct AckDTO {
    check_id: String,
    note: String,
    flagged: i64,
    acknowledged_by: String,
    acknowledged_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
}

/// Acknowledges a failing check (admin or security): it leaves the score, the
/// counts and the alarms until it expires or flags more items than now.
pub(super) async fn put_acknowledgement(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if !may_refresh(&h, &parts) {
        return write_error(
            StatusCode::FORBIDDEN,
            "acknowledging a check requires admin or security permission",
        );
    }
    let Some((analyzer, _)) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<AckReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let note = req.note.trim().to_string();
    if note.is_empty() || note.chars().count() > 1000 {
        return write_error(
            StatusCode::BAD_REQUEST,
            "a note of 1 to 1000 characters is required",
        );
    }
    let days = req.days.unwrap_or(30);
    if !(1..=MAX_ACK_DAYS).contains(&days) {
        return write_error(
            StatusCode::BAD_REQUEST,
            &format!("days must be from 1 to {MAX_ACK_DAYS}"),
        );
    }
    let report = match analyzer.latest().await {
        Ok(Some(report)) => report,
        Ok(None) => return write_error(StatusCode::BAD_REQUEST, "there is no analysis yet"),
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if !report.checks.iter().any(|c| c.id == id) {
        return write_error(StatusCode::NOT_FOUND, "no such check");
    }
    let Some(finding) = report.findings.iter().find(|f| f.id == id) else {
        return write_error(
            StatusCode::BAD_REQUEST,
            "only a failing check can be acknowledged",
        );
    };
    let now = Utc::now();
    let ack = crate::meta::InsightAck {
        check_id: id,
        note,
        flagged: finding.total,
        acknowledged_by: principal_name(&parts),
        acknowledged_at: now,
        expires_at: now + TimeDelta::days(days),
    };
    if let Err(err) = h.store.upsert_insight_ack(ack.clone()).await {
        return map_error(err);
    }
    h.audit(&parts, "", "insights.acknowledge", 200);
    write_json(
        StatusCode::OK,
        AckDTO {
            check_id: ack.check_id,
            note: ack.note,
            flagged: ack.flagged,
            acknowledged_by: ack.acknowledged_by,
            acknowledged_at: ack.acknowledged_at,
            expires_at: ack.expires_at,
        },
    )
}

/// Removes a check's acknowledgement (admin or security).
pub(super) async fn delete_acknowledgement(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if !may_refresh(&h, &parts) {
        return write_error(
            StatusCode::FORBIDDEN,
            "acknowledging a check requires admin or security permission",
        );
    }
    if let Err(err) = h.store.delete_insight_ack(&id).await {
        return map_error(err);
    }
    h.audit(&parts, "", "insights.unacknowledge", 200);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct HistoryQuery {
    #[serde(default)]
    days: Option<i64>,
}

#[derive(Debug, Serialize)]
struct CheckPointDTO {
    generated_at: chrono::DateTime<Utc>,
    status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    severity: String,
    flagged: i64,
}

#[derive(Debug, Serialize)]
struct CheckHistoryDTO {
    points: Vec<CheckPointDTO>,
}

/// Serves one check's flagged count and status per run. Only for callers who
/// see the whole report: the stored counts span every repository, so they
/// cannot be narrowed for anyone else.
pub(super) async fn check_history(
    State(h): State<Arc<Handler>>,
    UrlPath(id): UrlPath<String>,
    axum::extract::Query(query): axum::extract::Query<HistoryQuery>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    if !sees_everything(&h, &parts) {
        return write_error(
            StatusCode::FORBIDDEN,
            "check history requires admin, audit or security permission",
        );
    }
    let Some((analyzer, _)) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    let days = query.days.unwrap_or(30);
    if !(1..=90).contains(&days) {
        return write_error(StatusCode::BAD_REQUEST, "days must be from 1 to 90");
    }
    match analyzer
        .check_history(&id, Utc::now() - TimeDelta::days(days))
        .await
    {
        Ok(points) => write_json(
            StatusCode::OK,
            CheckHistoryDTO {
                points: points
                    .into_iter()
                    .map(|p| CheckPointDTO {
                        generated_at: p.generated_at,
                        status: p.status,
                        severity: p.severity,
                        flagged: p.flagged,
                    })
                    .collect(),
            },
        ),
        Err(err) => write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Debug, Serialize)]
struct InsightSettingsDTO {
    auto_analyze_enabled: bool,
    interval_minutes: i64,
    report_enabled: bool,
    receiver: String,
    report_cron: String,
    timezone: String,
    min_severity: &'static str,
    skip_when_unchanged: bool,
    updated_by: String,
    /// Empty before the first save.
    updated_at: String,
    /// Empty while the scheduled analysis is off.
    next_analysis_at: String,
    /// Empty while the scheduled alarm is off or has no receiver.
    next_report_at: String,
}

fn rfc3339(t: Option<chrono::DateTime<Utc>>) -> String {
    t.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

async fn settings_dto(
    analyzer: &crate::insights::Analyzer,
    s: crate::insights::Settings,
) -> Result<InsightSettingsDTO, Box<Response>> {
    let now = Utc::now();
    let next_analysis_at = analyzer.next_analysis_at(&s, now).await.map_err(|err| {
        Box::new(write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &err.to_string(),
        ))
    })?;
    let next_report_at = (s.report_enabled && !s.receiver.is_empty())
        .then(|| crate::coverage::parse_schedule(&s.report_cron, &s.timezone).ok())
        .flatten()
        .and_then(|schedule| schedule.next(now));
    Ok(InsightSettingsDTO {
        auto_analyze_enabled: s.auto_analyze_enabled,
        interval_minutes: s.interval_minutes,
        report_enabled: s.report_enabled,
        receiver: s.receiver,
        report_cron: s.report_cron,
        timezone: s.timezone,
        min_severity: s.min_severity.label(),
        skip_when_unchanged: s.skip_when_unchanged,
        updated_by: s.updated_by,
        updated_at: rfc3339(s.updated_at),
        next_analysis_at: rfc3339(next_analysis_at),
        next_report_at: rfc3339(next_report_at),
    })
}

/// Returns the insight settings (admin or security).
pub(super) async fn get_settings(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let Some((analyzer, _)) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    if !may_refresh(&h, &parts) {
        return write_error(StatusCode::FORBIDDEN, "forbidden");
    }
    let settings = match analyzer.settings().await {
        Ok(settings) => settings,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    match settings_dto(&analyzer, settings).await {
        Ok(dto) => write_json(StatusCode::OK, dto),
        Err(response) => *response,
    }
}

/// The settings write body. An omitted field keeps its stored value.
#[derive(Debug, Default, serde::Deserialize)]
struct InsightSettingsReq {
    auto_analyze_enabled: Option<bool>,
    interval_minutes: Option<i64>,
    report_enabled: Option<bool>,
    receiver: Option<String>,
    report_cron: Option<String>,
    timezone: Option<String>,
    min_severity: Option<String>,
    skip_when_unchanged: Option<bool>,
}

/// Validates and stores the insight settings (admin or security).
pub(super) async fn update_settings(State(h): State<Arc<Handler>>, request: Request) -> Response {
    use crate::insights::settings::{MAX_INTERVAL_MINUTES, MIN_INTERVAL_MINUTES, parse_severity};

    let (parts, body) = request.into_parts();
    let Some((analyzer, _)) = h.insights() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security insights are disabled",
        );
    };
    if !may_refresh(&h, &parts) {
        return write_error(StatusCode::FORBIDDEN, "forbidden");
    }
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<InsightSettingsReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let mut s = match analyzer.settings().await {
        Ok(settings) => settings,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if let Some(v) = req.auto_analyze_enabled {
        s.auto_analyze_enabled = v;
    }
    if let Some(v) = req.interval_minutes {
        if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&v) {
            return write_error(
                StatusCode::BAD_REQUEST,
                &format!(
                    "interval_minutes must be between {MIN_INTERVAL_MINUTES} and {MAX_INTERVAL_MINUTES}"
                ),
            );
        }
        s.interval_minutes = v;
    }
    if let Some(v) = req.report_enabled {
        s.report_enabled = v;
    }
    if let Some(cron) = &req.report_cron {
        s.report_cron = cron.trim().to_string();
    }
    if let Some(timezone) = &req.timezone {
        s.timezone = timezone.trim().to_string();
    }
    if let Err(err) = crate::coverage::parse_schedule(&s.report_cron, &s.timezone) {
        return write_error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    if let Some(receiver) = &req.receiver {
        let name = receiver.trim().to_string();
        if !name.is_empty()
            && let Err(msg) = h.validate_receiver_name(&name).await
        {
            return write_error(StatusCode::BAD_REQUEST, &msg);
        }
        s.receiver = name;
    }
    if let Some(raw) = &req.min_severity {
        let Some(severity) = parse_severity(raw.trim()) else {
            return write_error(
                StatusCode::BAD_REQUEST,
                "min_severity must be one of critical, high, medium, low, info",
            );
        };
        s.min_severity = severity;
    }
    if let Some(v) = req.skip_when_unchanged {
        s.skip_when_unchanged = v;
    }
    s.updated_by = principal_name(&parts);
    s.updated_at = Some(Utc::now());
    if let Err(err) = analyzer.save_settings(&s).await {
        return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    h.audit(&parts, "", "insights.settings.update", 200);
    match settings_dto(&analyzer, s).await {
        Ok(dto) => write_json(StatusCode::OK, dto),
        Err(response) => *response,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::insights::{Affected, Audience, Check, CheckStatus, Finding, Inventory, Severity};

    fn finding(id: &str, category: Category, repos: &[&str]) -> Finding {
        let affected: Vec<Affected> = repos
            .iter()
            .map(|r| Affected {
                kind: "repository".to_string(),
                label: (*r).to_string(),
                repo: (*r).to_string(),
                ..Default::default()
            })
            .collect();
        let by_repo: BTreeMap<String, i64> = repos.iter().map(|r| ((*r).to_string(), 1)).collect();
        Finding {
            id: id.to_string(),
            code: String::new(),
            severity: Severity::High,
            category,
            audiences: vec![Audience::Developer],
            title: String::new(),
            summary: String::new(),
            rationale: String::new(),
            criteria: String::new(),
            recommendation: String::new(),
            params: BTreeMap::from([
                ("count".to_string(), repos.len().to_string()),
                ("hosted".to_string(), "2".to_string()),
            ]),
            total: repos.len() as i64,
            affected,
            by_repo,
            everyone: false,
            acknowledged: false,
        }
    }

    fn check(id: &str, category: Category) -> Check {
        Check {
            id: id.to_string(),
            code: String::new(),
            category,
            audiences: Vec::new(),
            title: String::new(),
            rationale: String::new(),
            criteria: String::new(),
            recommendation: String::new(),
            status: CheckStatus::Action,
            flagged: 1,
            acknowledgement: None,
        }
    }

    #[test]
    fn narrow_keeps_readable_repositories_only() {
        let mut report = Report::new(
            Utc::now(),
            1,
            "test",
            Inventory::default(),
            vec![
                finding(
                    "public-repositories",
                    Category::Exposure,
                    &["team-a", "team-b"],
                ),
                finding("other-team", Category::Policy, &["team-b"]),
                finding("tokens-no-expiry", Category::Access, &[]),
                Finding {
                    everyone: true,
                    ..finding("coverage-bypass", Category::SupplyChain, &[])
                },
            ],
            vec![
                check("other-team", Category::Policy),
                check("tokens-no-expiry", Category::Access),
                check("public-repositories", Category::Exposure),
            ],
        );
        narrow(&mut report, |repo| repo == "team-a");
        let statuses: Vec<_> = report
            .checks
            .iter()
            .map(|c| (c.id.as_str(), c.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("other-team", CheckStatus::Ok),
                ("public-repositories", CheckStatus::Action)
            ],
            "access checks hidden, a check whose finding left the view passes"
        );
        assert_eq!(report.findings.len(), 2, "{:?}", report.findings);
        assert!(
            report.findings.iter().any(|f| f.id == "coverage-bypass"),
            "shared findings stay"
        );
        let f = report
            .findings
            .iter()
            .find(|f| f.id == "public-repositories")
            .expect("public finding");
        assert_eq!(f.total, 1);
        assert_eq!(f.affected.len(), 1);
        assert_eq!(f.params.get("count").map(String::as_str), Some("1"));
        assert!(
            !f.params.contains_key("hosted"),
            "repository-wide figures dropped"
        );
        assert_eq!(report.score, 100 - 2 * Severity::High.penalty());
    }
}
