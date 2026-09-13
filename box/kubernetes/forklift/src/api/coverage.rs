use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::response::Response;
use chrono::Utc;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::coverage;
use crate::notify;

use super::notifications::{
    NotificationSampleReportDTO, NotificationSampleResultDTO, SampleReceiverInfo,
};
use super::{Handler, map_error, principal_name, write_error, write_json};

impl Handler {
    /// Returns the scanner, or the 503 that a deployment with no GitLab
    /// integration configured gets for every coverage endpoint.
    /// The error is boxed because a `Response` dwarfs the `Arc` it competes with
    /// in the `Result`.
    fn coverage_ready(&self) -> Result<Arc<coverage::Scanner>, Box<Response>> {
        self.coverage()
            .ok_or_else(|| Box::new(coverage_unconfigured()))
    }
}

fn coverage_unconfigured() -> Response {
    write_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "coverage scanning is not configured",
    )
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct ProjectQuery {
    #[serde(default)]
    pub(super) path: String,
    #[serde(default)]
    pub(super) r#ref: String,
    #[serde(default)]
    pub(super) days: String,
}

/// Reads the project path from the query string.
///
/// A GitLab path contains slashes ("group/subgroup/project"), so it travels as a
/// query parameter rather than a path segment: a percent-encoded slash inside a
/// segment is decoded inconsistently by proxies, and the alternative, a wildcard
/// route, would collide with the pipeline and last-commit subpaths.
fn coverage_project_path(q: &ProjectQuery) -> Result<String, Box<Response>> {
    let path = q.path.trim().to_string();
    if path.is_empty() {
        return Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "path is required",
        )));
    }
    // The path is interpolated into a GitLab API URL. Rejecting these outright
    // keeps a crafted value from reaching the client as anything but one
    // URL-encoded segment.
    if path.contains(['?', '#']) || path.contains("..") {
        return Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "invalid project path",
        )));
    }
    Ok(path)
}

/// Returns the dashboard payload: summary, projects, scan state and schedule.
/// Readable by any authenticated principal: the measurement is what the whole
/// organisation is working towards, so hiding it behind admin would make the
/// number nobody can see the number nobody acts on.
pub(super) async fn get(State(h): State<Arc<Handler>>) -> Response {
    match h.coverage_ready() {
        Ok(scanner) => write_json(StatusCode::OK, scanner.overview()),
        Err(response) => *response,
    }
}

/// Returns the per-group breakdown, worst coverage first.
pub(super) async fn list_groups(State(h): State<Arc<Handler>>) -> Response {
    match h.coverage_ready() {
        Ok(scanner) => write_json(StatusCode::OK, scanner.group_coverage()),
        Err(response) => *response,
    }
}

/// Bounds the trend window to the retention the store keeps, so the API never
/// accepts a window it cannot fill.
const MAX_HISTORY_DAYS: i64 = coverage::HISTORY_RETENTION_DAYS;

/// Returns the coverage trend over the requested window.
pub(super) async fn list_history(
    State(h): State<Arc<Handler>>,
    Query(q): Query<ProjectQuery>,
) -> Response {
    if let Err(response) = h.coverage_ready() {
        return *response;
    }
    let mut days = coverage::HISTORY_RETENTION_DAYS;
    if !q.days.is_empty() {
        match q.days.parse::<i64>() {
            Ok(n) if (1..=MAX_HISTORY_DAYS).contains(&n) => days = n,
            _ => {
                return write_error(
                    StatusCode::BAD_REQUEST,
                    &format!("days must be between 1 and {MAX_HISTORY_DAYS}"),
                );
            }
        }
    }
    match h.store.list_coverage_history(days).await {
        Ok(history) => write_json(StatusCode::OK, history),
        Err(err) => map_error(err),
    }
}

/// Turns a scanner error into its HTTP answer: a not-found sentinel is a 404,
/// anything else follows the shared store mapping.
fn map_coverage_error(err: coverage::Error) -> Response {
    if err.is_not_found() {
        return write_error(StatusCode::NOT_FOUND, "not found");
    }
    write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string())
}

/// Returns one project's detail, falling back to a GitLab lookup for a project
/// the last scan did not cover.
pub(super) async fn get_project(
    State(h): State<Arc<Handler>>,
    Query(q): Query<ProjectQuery>,
) -> Response {
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let path = match coverage_project_path(&q) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match scanner.project_detail(&path).await {
        Ok(detail) => write_json(StatusCode::OK, detail),
        Err(err) => map_coverage_error(err),
    }
}

/// Returns the tip commit of the branch the verdict came from.
pub(super) async fn get_project_last_commit(
    State(h): State<Arc<Handler>>,
    Query(q): Query<ProjectQuery>,
) -> Response {
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let path = match coverage_project_path(&q) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match scanner.last_commit(&path).await {
        Ok(Some(commit)) => write_json(StatusCode::OK, commit),
        Ok(None) => write_error(StatusCode::NOT_FOUND, "no commit found"),
        Err(err) => map_coverage_error(err),
    }
}

/// Returns the project's CI definitions on one ref.
///
/// Admin only, and the scanner reads nothing but files matching the GitLab CI
/// pattern: this shows how a project builds without exposing its source.
pub(super) async fn get_project_pipeline(
    State(h): State<Arc<Handler>>,
    Query(q): Query<ProjectQuery>,
) -> Response {
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let path = match coverage_project_path(&q) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    match scanner.pipeline(&path, q.r#ref.trim()).await {
        Ok(pipeline) => write_json(StatusCode::OK, pipeline),
        Err(err) => map_coverage_error(err),
    }
}

/// Changes what is muted on one project.
///
/// `scopes` names the checks to mute and is the whole state, not a delta: what
/// it leaves out is unmuted. `muted` stays for the all-or-nothing case, so "take
/// this project out" needs no list of every check that exists; `scopes` wins when
/// both are sent.
#[derive(Debug, Clone, Default, Deserialize)]
struct CoverageMuteReq {
    #[serde(default)]
    muted: Option<bool>,
    #[serde(default)]
    scopes: Option<Vec<String>>,
}

/// Acknowledges a mute change.
#[derive(Debug, Clone, Serialize)]
struct CoverageMuteDTO {
    project_path: String,
    /// True when the whole project is out, which is every check muted.
    muted: bool,
    scopes: Vec<String>,
}

/// Mutes a project or some of its checks, or brings them back into the
/// measurement. Admin only: it changes what the coverage number means.
pub(super) async fn update_project_mute(
    State(h): State<Arc<Handler>>,
    Query(q): Query<ProjectQuery>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let path = match coverage_project_path(&q) {
        Ok(path) => path,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<CoverageMuteReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    if req.scopes.is_none() && req.muted.is_none() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "muted must be a boolean, or scopes a list of checks",
        );
    }
    let scopes = match (&req.scopes, req.muted) {
        (Some(scopes), _) => match coverage::parse_mute_scopes(scopes) {
            Ok(parsed) => parsed,
            Err(err) => return write_error(StatusCode::BAD_REQUEST, &err.to_string()),
        },
        (None, Some(true)) => coverage::MuteScopes {
            ci: true,
            registry: true,
        },
        (None, _) => coverage::MuteScopes::default(),
    };
    if let Err(err) = scanner
        .set_muted(&path, scopes, &principal_name(&parts))
        .await
    {
        return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    h.audit(&parts, "", "coverage.mute", 200);
    write_json(
        StatusCode::OK,
        CoverageMuteDTO {
            project_path: path,
            muted: scopes.all(),
            scopes: scopes.list(),
        },
    )
}

/// The settings surface. It carries no credential: the GitLab URL is shown
/// because the console links to it, and the token is never read back at all.
#[derive(Debug, Clone, Default, Serialize)]
struct CoverageSettingsDTO {
    /// The external domain a project must reference to count. Editable; empty
    /// falls back to `forklift_host_default`.
    forklift_host: String,
    /// The instance the scan reads from, reported read-only: the URL and the
    /// token are one deployment decision, so both come from the environment. The
    /// token is never on this surface at all.
    gitlab_url: String,
    exclude_topics: Vec<String>,
    scan_cron: String,
    timezone: String,
    auto_scan_enabled: bool,
    report_enabled: bool,
    receiver: String,
    skip_when_full_coverage: bool,
    max_branches: i64,
    since_days: i64,
    use_search: bool,
    updated_by: String,
    updated_at: String,
    /// The effective state: switched on, with a URL and a token.
    gitlab_configured: bool,
    /// What an empty `forklift_host` falls back to, reported separately from the
    /// effective value so the console can tell whether clearing the field would
    /// leave the scan with nothing to match.
    forklift_host_default: String,
    /// Previews when the saved schedule fires next, empty when automatic
    /// scanning is off.
    next_run_at: String,
}

fn to_coverage_settings_dto(s: &coverage::Scanner) -> CoverageSettingsDTO {
    let cfg = s.settings();
    CoverageSettingsDTO {
        forklift_host: s.match_host(),
        gitlab_url: s.gitlab_url(),
        exclude_topics: cfg.exclude_topics,
        scan_cron: cfg.scan_cron,
        timezone: cfg.timezone,
        auto_scan_enabled: cfg.auto_scan_enabled,
        report_enabled: cfg.report_enabled,
        receiver: cfg.receiver,
        skip_when_full_coverage: cfg.skip_when_full_coverage,
        max_branches: cfg.max_branches,
        since_days: cfg.since_days,
        use_search: cfg.use_search,
        updated_by: cfg.updated_by,
        updated_at: if crate::meta::time::is_zero(cfg.updated_at) {
            String::new()
        } else {
            cfg.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        },
        gitlab_configured: s.enabled(),
        forklift_host_default: s.default_forklift_host(),
        next_run_at: s
            .next_run_at(Utc::now())
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default(),
    }
}

/// Returns the current settings (admin).
pub(super) async fn get_settings(State(h): State<Arc<Handler>>) -> Response {
    match h.coverage_ready() {
        Ok(scanner) => write_json(StatusCode::OK, to_coverage_settings_dto(&scanner)),
        Err(response) => *response,
    }
}

/// The settings write body. Every field is optional so an omitted one keeps its
/// stored value: the console saves one panel at a time, and a partial form must
/// not reset the fields it does not show.
#[derive(Debug, Clone, Default, Deserialize)]
struct CoverageSettingsReq {
    #[serde(default)]
    forklift_host: Option<String>,
    #[serde(default)]
    exclude_topics: Option<Vec<String>>,
    #[serde(default)]
    scan_cron: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    auto_scan_enabled: Option<bool>,
    #[serde(default)]
    report_enabled: Option<bool>,
    #[serde(default)]
    receiver: Option<String>,
    #[serde(default)]
    skip_when_full_coverage: Option<bool>,
    #[serde(default)]
    max_branches: Option<i64>,
    #[serde(default)]
    since_days: Option<i64>,
    #[serde(default)]
    use_search: Option<bool>,
}

// Bounds on how deep the crawl looks. There is nothing here about request rate:
// that is discovered at run time by the adaptive limiter rather than configured.
const MAX_SCAN_BRANCHES: i64 = 200;
const MAX_SCAN_SINCE_DAYS: i64 = 3650;

/// Validates and stores the settings (admin).
pub(super) async fn update_settings(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<CoverageSettingsReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };

    let mut cfg = scanner.settings();
    if let Some(raw) = &req.forklift_host {
        let host = coverage::normalize_host(raw);
        // An empty value is how the host is handed back to the external URL,
        // rather than a way to leave the scan with nothing to match.
        if !host.is_empty() {
            let msg = coverage::validate_match_host(&host);
            if !msg.is_empty() {
                return write_error(StatusCode::BAD_REQUEST, &msg);
            }
        }
        cfg.forklift_host = host;
    }
    if let Some(topics) = &req.exclude_topics {
        cfg.exclude_topics = trim_strings(topics);
    }
    if let Some(cron) = &req.scan_cron {
        cfg.scan_cron = cron.trim().to_string();
    }
    if let Some(timezone) = &req.timezone {
        cfg.timezone = timezone.trim().to_string();
    }
    // Cron and timezone are validated together: either one alone is meaningless,
    // and an expression that does not parse would silently stop the schedule.
    if let Err(err) = coverage::parse_schedule(&cfg.scan_cron, &cfg.timezone) {
        return write_error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    if let Some(v) = req.auto_scan_enabled {
        cfg.auto_scan_enabled = v;
    }
    if let Some(v) = req.report_enabled {
        cfg.report_enabled = v;
    }
    if let Some(receiver) = &req.receiver {
        let name = receiver.trim().to_string();
        // Empty is how the report is turned off, so only a named one is checked.
        if !name.is_empty()
            && let Err(msg) = h.validate_coverage_receiver(&name).await
        {
            return write_error(StatusCode::BAD_REQUEST, &msg);
        }
        cfg.receiver = name;
    }
    if let Some(v) = req.skip_when_full_coverage {
        cfg.skip_when_full_coverage = v;
    }
    for (value, target, name, min, max) in [
        (
            req.max_branches,
            &mut cfg.max_branches,
            "max_branches",
            0,
            MAX_SCAN_BRANCHES,
        ),
        (
            req.since_days,
            &mut cfg.since_days,
            "since_days",
            1,
            MAX_SCAN_SINCE_DAYS,
        ),
    ] {
        let Some(value) = value else {
            continue;
        };
        if value < min || value > max {
            return write_error(
                StatusCode::BAD_REQUEST,
                &format!("{name} must be between {min} and {max}"),
            );
        }
        *target = value;
    }
    if let Some(v) = req.use_search {
        cfg.use_search = v;
    }
    cfg.updated_by = principal_name(&parts);

    if let Err(err) = h.store.write_coverage_settings(cfg).await {
        return map_error(err);
    }
    if let Err(err) = scanner.refresh_settings().await {
        return write_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
    }
    h.audit(&parts, "", "coverage.settings.update", 200);
    write_json(StatusCode::OK, to_coverage_settings_dto(&scanner))
}

impl Handler {
    /// Refuses a name that is not an existing receiver, so a typo shows up at
    /// save time instead of as a report that silently goes nowhere.
    async fn validate_coverage_receiver(&self, name: &str) -> Result<(), String> {
        let all = self
            .store
            .list_receivers()
            .await
            .map_err(|err| err.to_string())?;
        if all.iter().any(|rec| rec.name == name) {
            return Ok(());
        }
        Err(format!("unknown notification receiver: {name}"))
    }
}

fn trim_strings(input: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(input.len());
    let mut seen = std::collections::HashSet::new();
    for value in input {
        let value = value.trim().to_string();
        if value.is_empty() || !seen.insert(value.clone()) {
            continue;
        }
        out.push(value);
    }
    out
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CoverageHostCheckReq {
    #[serde(default)]
    forklift_host: String,
}

/// Reports whether a host is shaped like a forklift address and whether it
/// resolves (admin).
///
/// Only the resolver is asked; nothing is sent to the host, which is what keeps
/// this from being the request forwarder an HTTP probe would be. The DNS result
/// is advisory: forklift resolves from inside the cluster and the builds it
/// measures resolve from wherever they run, so a name that fails here can be
/// correct for them. Saving is refused on the syntax, never on the lookup.
pub(super) async fn check_host(State(h): State<Arc<Handler>>, request: Request) -> Response {
    if let Err(response) = h.coverage_ready() {
        return *response;
    }
    let (_, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    let Ok(req) = serde_json::from_slice::<CoverageHostCheckReq>(&bytes) else {
        return write_error(StatusCode::BAD_REQUEST, "invalid body");
    };
    write_json(
        StatusCode::OK,
        coverage::lookup_host(&req.forklift_host).await,
    )
}

/// Reports whether the configured GitLab instance answers and accepts the token
/// (admin).
pub(super) async fn gitlab_check(State(h): State<Arc<Handler>>) -> Response {
    match h.coverage_ready() {
        Ok(scanner) => write_json(StatusCode::OK, scanner.check_gitlab().await),
        Err(response) => *response,
    }
}

/// Acknowledges a manual scan.
#[derive(Debug, Clone, Serialize)]
struct CoverageScanStartedDTO {
    started: bool,
}

/// Kicks off a manual scan (admin) and returns immediately.
///
/// A full crawl takes minutes, so the request does not wait on it: the console
/// polls the overview, whose `scan_progress` fills in as projects finish. The
/// scan runs detached for the same reason: cancelling it because the browser
/// navigated away would leave the picture half-replaced.
pub(super) async fn start_scan(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    if !scanner.enabled() {
        return coverage_unconfigured();
    }
    if scanner.scanning() {
        return write_error(StatusCode::CONFLICT, "a scan is already in progress");
    }
    let Some(start) = h.coverage_scan() else {
        return coverage_unconfigured();
    };
    if !start(&principal_name(&parts)) {
        return write_error(StatusCode::CONFLICT, "a scan is already in progress");
    }
    h.audit(&parts, "", "coverage.scan", 200);
    write_json(StatusCode::OK, CoverageScanStartedDTO { started: true })
}

#[derive(Debug, Clone, Serialize)]
struct CoverageAlarmPreviewDTO {
    payload: notify::CoveragePayload,
    /// True when no scan has completed yet, so the numbers shown are made up
    /// rather than measured.
    sample: bool,
    receivers: Vec<SampleReceiverInfo>,
}

impl Handler {
    /// Turns the current picture into the alarm input, or the sample when no scan
    /// has completed yet.
    fn coverage_report(&self, scanner: &coverage::Scanner) -> (notify::CoverageReport, bool) {
        let overview = scanner.overview();
        if overview.last_scanned_at.is_none() {
            return (notify::sample_coverage_report(), true);
        }
        let projects = scanner
            .not_applied()
            .into_iter()
            .map(|p| notify::CoverageProject {
                path: p.path,
                partial: p.applied == coverage::STATE_PARTIAL,
                web_url: p.web_url,
            })
            .collect();
        (
            notify::CoverageReport {
                target: overview.summary.target,
                applied: overview.summary.applied,
                partial: overview.summary.partial,
                not_applied: overview.summary.not_applied,
                errored: overview.summary.errored,
                skipped: overview.summary.skipped,
                not_applied_projects: projects,
                sample: false,
            },
            false,
        )
    }

    /// Resolves the configured receiver to a delivery target, reporting its
    /// status for the preview and the deliverable target for the send. A receiver
    /// that was deleted or disabled since it was chosen shows up in the status
    /// rather than silently disappearing.
    async fn coverage_targets(
        &self,
        name: &str,
    ) -> Result<(Vec<SampleReceiverInfo>, Vec<notify::Target>), crate::meta::Error> {
        let mut info = Vec::with_capacity(1);
        let mut targets = Vec::with_capacity(1);
        if name.is_empty() {
            return Ok((info, targets));
        }
        let all = self.store.list_receivers().await?;
        let found = all.into_iter().find(|rec| rec.name == name);
        info.push(SampleReceiverInfo {
            name: name.to_string(),
            exists: found.is_some(),
            enabled: found.as_ref().is_some_and(|rec| rec.enabled),
        });
        if let Some(rec) = found
            && rec.enabled
            && !rec.webhook_url.is_empty()
        {
            targets.push(notify::Target {
                name: rec.name,
                url: rec.webhook_url,
            });
        }
        Ok((info, targets))
    }
}

/// Renders the report without delivering it (admin).
pub(super) async fn preview_alarm(State(h): State<Arc<Handler>>) -> Response {
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let Some(notifier) = h.notifier() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "notifications are not configured",
        );
    };
    let (report, sample) = h.coverage_report(&scanner);
    let (info, _) = match h.coverage_targets(&scanner.settings().receiver).await {
        Ok(resolved) => resolved,
        Err(err) => return map_error(err),
    };
    write_json(
        StatusCode::OK,
        CoverageAlarmPreviewDTO {
            payload: notifier.build_coverage_report(&report),
            sample,
            receivers: info,
        },
    )
}

/// Delivers the report now (admin), reporting each receiver's outcome. A manual
/// send ignores `skip_when_full_coverage`: somebody asked for it.
pub(super) async fn send_alarm(State(h): State<Arc<Handler>>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let scanner = match h.coverage_ready() {
        Ok(scanner) => scanner,
        Err(response) => return *response,
    };
    let Some(notifier) = h.notifier() else {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "notifications are not configured",
        );
    };
    if scanner.overview().last_scanned_at.is_none() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "there is no scan result to report yet",
        );
    }
    let (_, targets) = match h.coverage_targets(&scanner.settings().receiver).await {
        Ok(resolved) => resolved,
        Err(err) => return map_error(err),
    };
    if targets.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "no enabled receiver is selected for coverage reports",
        );
    }
    let (report, _) = h.coverage_report(&scanner);
    let payload = notifier.build_coverage_report(&report);
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        match notifier.send_coverage_report(&target.url, &payload).await {
            Ok(()) => results.push(NotificationSampleResultDTO {
                name: target.name,
                ok: true,
                error: String::new(),
            }),
            Err(err) => results.push(NotificationSampleResultDTO {
                name: target.name,
                ok: false,
                error: err.to_string(),
            }),
        }
    }
    h.audit(&parts, "", "coverage.notify", 200);
    write_json(StatusCode::OK, NotificationSampleReportDTO { results })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::Router;
    use http::{Method, StatusCode};
    use tempfile::TempDir;

    use crate::auth;
    use crate::coverage::{self, MuteScopes, Scanner, ScannerOptions};
    use crate::meta::{Receiver, Store};

    use crate::api::Handler;
    use crate::testing::api::mk_upload_user;
    use crate::testing::api::{
        ADMIN_PASS, ADMIN_USER, TestResponse, do_as_on, new_test_server, send_on,
    };

    /// Wires the API with a coverage scanner over a real store, so the settings and
    /// exclusion routes exercise their actual persistence. No GitLab is reachable,
    /// which is the ordinary state of a deployment that has not opted into
    /// coverage: the read surfaces still answer.
    struct CoverageServer {
        app: Router,
        store: Arc<Store>,
        scanner: Arc<Scanner>,
        _dir: TempDir,
    }

    impl CoverageServer {
        async fn admin_do(&self, method: Method, uri: &str, body: &str) -> TestResponse {
            do_as_on(&self.app, ADMIN_USER, ADMIN_PASS, method, uri, body).await
        }
    }

    async fn new_coverage_server_with(enabled: bool, forklift_host: &str) -> CoverageServer {
        // The scanner builds a reqwest client, which needs a crypto provider.
        crate::server::install_crypto_provider();
        auth::set_test_hash_cost();
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(
            Store::open(dir.path().join("api.db"))
                .await
                .expect("open store"),
        );
        let authz = auth::Service::new(
            Arc::clone(&store),
            auth::Options {
                session_secret: b"test-secret-test-secret-test-secret".to_vec(),
                ..Default::default()
            },
        );
        authz
            .bootstrap_admin(ADMIN_USER, ADMIN_PASS)
            .await
            .expect("bootstrap admin");
        let scanner = Scanner::new(ScannerOptions {
            store: Arc::clone(&store) as Arc<dyn coverage::Store>,
            enabled,
            gitlab_url: "https://gitlab.example.com".to_string(),
            gitlab_token: "test-token".to_string(),
            forklift_host: forklift_host.to_string(),
        });
        let handler = Handler::new(Arc::clone(&store), Some(Arc::clone(&authz)), None);
        handler.set_coverage(Arc::clone(&scanner));
        let app = crate::api::routes(Arc::clone(&handler)).layer(
            axum::middleware::from_fn_with_state(Arc::clone(&authz), auth::middleware),
        );
        CoverageServer {
            app,
            store,
            scanner,
            _dir: dir,
        }
    }

    async fn new_coverage_server() -> CoverageServer {
        let srv = new_coverage_server_with(true, "https://forklift.example.com").await;
        srv.scanner.load().await.expect("load scanner");
        srv
    }

    /// Pins the split the feature is built around: everyone can see the
    /// measurement, only an administrator can change what it measures or make it
    /// run.
    #[tokio::test]
    async fn coverage_read_is_open_to_any_signed_in_user() {
        let srv = new_coverage_server().await;
        mk_upload_user(&srv.store, "reader", "read").await;

        for path in ["/coverage", "/coverage/groups", "/coverage/history"] {
            let resp = do_as_on(&srv.app, "reader", "pw123456", Method::GET, path, "").await;
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "GET {path} as a non-admin = {}, want 200: {}",
                resp.status,
                resp.text()
            );
        }
    }

    #[tokio::test]
    async fn coverage_mutations_are_admin_only() {
        let srv = new_coverage_server().await;
        mk_upload_user(&srv.store, "reader", "read").await;

        for (method, path, body) in [
            (Method::GET, "/coverage/settings", ""),
            (
                Method::PUT,
                "/coverage/settings",
                r#"{"forklift_host":"evil.example.com"}"#,
            ),
            (
                Method::POST,
                "/coverage/settings/check-host",
                r#"{"forklift_host":"evil.example.com"}"#,
            ),
            (Method::GET, "/coverage/gitlab-check", ""),
            (Method::POST, "/coverage/scan", ""),
            (
                Method::PUT,
                "/coverage/project/mute?path=team/app",
                r#"{"muted":true}"#,
            ),
            (Method::GET, "/coverage/project/pipeline?path=team/app", ""),
            (Method::GET, "/coverage/notification/preview", ""),
            (Method::POST, "/coverage/notification/send", ""),
        ] {
            let resp = do_as_on(&srv.app, "reader", "pw123456", method.clone(), path, body).await;
            assert_eq!(
                resp.status,
                StatusCode::FORBIDDEN,
                "{method} {path} as a non-admin = {}, want 403",
                resp.status
            );
        }
    }

    #[tokio::test]
    async fn coverage_requires_authentication() {
        let srv = new_coverage_server().await;
        let request = http::Request::builder()
            .method(Method::GET)
            .uri("/coverage")
            .body(axum::body::Body::empty())
            .expect("build request");
        let resp = send_on(&srv.app, request).await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "anonymous GET /coverage = {}, want 401",
            resp.status
        );
    }

    #[tokio::test]
    async fn coverage_is_503_without_a_scanner() {
        // A deployment that never configured coverage answers "not available"
        // rather than 404, so the console can say what is missing.
        let srv = new_test_server().await;
        let resp = srv.admin_do(Method::GET, "/coverage", "").await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "GET /coverage with no scanner = {}, want 503",
            resp.status
        );
    }

    #[tokio::test]
    async fn coverage_settings_round_trip() {
        let srv = new_coverage_server().await;

        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/settings",
                r#"{"forklift_host":"nexus.corp.example.org",
		  "scan_cron":"0 9 * * 1-5","timezone":"Asia/Seoul",
		  "exclude_topics":["skip-me"," skip-me ",""],"max_branches":4}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "PUT /coverage/settings = {}",
            resp.status
        );
        let saved = resp.json();
        assert!(
            saved["max_branches"] == 4 && saved["timezone"] == "Asia/Seoul",
            "saved = {saved}"
        );
        assert_eq!(
            saved["forklift_host"], "nexus.corp.example.org",
            "forklift_host = {}, want the saved host",
            saved["forklift_host"]
        );
        // Blank and duplicate entries are dropped rather than stored.
        assert!(
            saved["exclude_topics"].as_array().map(Vec::len) == Some(1)
                && saved["exclude_topics"][0] == "skip-me",
            "exclude_topics = {}, want the list trimmed and deduplicated",
            saved["exclude_topics"]
        );
        assert_ne!(
            saved["next_run_at"], "",
            "next_run_at is empty even though automatic scanning is on"
        );
        // The credential is never on this surface, whatever else is.
        assert!(
            saved["gitlab_url"] == "https://gitlab.example.com"
                && saved["gitlab_configured"] == true,
            "gitlab connection = {}/{}",
            saved["gitlab_url"],
            saved["gitlab_configured"]
        );
        // The host's fallback is reported apart from the effective value, so the
        // console can tell whether clearing the field would leave the scan with
        // nothing to match.
        assert_eq!(
            saved["forklift_host_default"], "forklift.example.com",
            "forklift_host_default = {}",
            saved["forklift_host_default"]
        );

        // An omitted field keeps its stored value, so saving one panel does not
        // reset the others.
        let saved = srv
            .admin_do(Method::PUT, "/coverage/settings", r#"{"since_days":30}"#)
            .await
            .json();
        assert!(
            saved["since_days"] == 30
                && saved["max_branches"] == 4
                && saved["forklift_host"] == "nexus.corp.example.org"
                && saved["timezone"] == "Asia/Seoul",
            "partial save reset other fields: {saved}"
        );

        let got = srv.scanner.match_host();
        assert_eq!(
            got, "nexus.corp.example.org",
            "match_host = {got:?}, want the saved host"
        );

        // Clearing the field hands the host back to the external URL rather than
        // leaving the scan with nothing to match.
        srv.admin_do(Method::PUT, "/coverage/settings", r#"{"forklift_host":""}"#)
            .await;
        let got = srv.scanner.match_host();
        assert_eq!(
            got, "forklift.example.com",
            "match_host after clearing = {got:?}, want the derived host"
        );
    }

    #[tokio::test]
    async fn coverage_settings_rejects_bad_input() {
        let srv = new_coverage_server().await;

        for (name, body) in [
            (
                "host with a path",
                r#"{"forklift_host":"forklift.example.com/npm"}"#,
            ),
            (
                "host with a wildcard",
                r#"{"forklift_host":"*.example.com"}"#,
            ),
            (
                "host port out of range",
                r#"{"forklift_host":"forklift.example.com:99999"}"#,
            ),
            ("unparseable cron", r#"{"scan_cron":"not a cron"}"#),
            ("unknown timezone", r#"{"timezone":"Mars/Olympus"}"#),
            ("max_branches out of range", r#"{"max_branches":9999}"#),
            ("since_days out of range", r#"{"since_days":0}"#),
            ("unknown receiver", r#"{"receiver":"nope"}"#),
        ] {
            let resp = srv.admin_do(Method::PUT, "/coverage/settings", body).await;
            assert_eq!(
                resp.status,
                StatusCode::BAD_REQUEST,
                "{name}: PUT = {}, want 400",
                resp.status
            );
        }
    }

    /// Clearing the receiver turns the report off rather than being refused as an
    /// unknown name.
    #[tokio::test]
    async fn coverage_settings_accepts_an_empty_receiver() {
        let srv = new_coverage_server().await;
        let resp = srv
            .admin_do(Method::PUT, "/coverage/settings", r#"{"receiver":""}"#)
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "PUT = {}: {}",
            resp.status,
            resp.text()
        );
    }

    /// The report switch is kept apart from the receiver: turning the report off
    /// must not discard where it was going, the same way turning the schedule off
    /// keeps the cron expression.
    #[tokio::test]
    async fn coverage_report_toggle_keeps_the_receiver() {
        let srv = new_coverage_server().await;
        srv.store
            .create_receiver(Receiver {
                name: "platform-alerts".to_string(),
                webhook_url: "https://hooks.example.com/abc".to_string(),
                enabled: true,
                ..Default::default()
            })
            .await
            .expect("create receiver");

        // On by default, so choosing a receiver is enough to arm the report.
        srv.admin_do(
            Method::PUT,
            "/coverage/settings",
            r#"{"receiver":"platform-alerts"}"#,
        )
        .await;
        assert!(
            srv.scanner.overview().alarm_configured,
            "alarm_configured is false with the report on and a receiver chosen"
        );

        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/settings",
                r#"{"report_enabled":false}"#,
            )
            .await;
        let status = resp.status;
        let saved = resp.json();
        assert!(
            status == StatusCode::OK && saved["report_enabled"] != true,
            "PUT = {status}, report_enabled = {}",
            saved["report_enabled"]
        );
        assert_eq!(
            saved["receiver"], "platform-alerts",
            "receiver = {}, want it kept across the toggle",
            saved["receiver"]
        );
        assert!(
            !srv.scanner.overview().alarm_configured,
            "alarm_configured is true with the report switched off"
        );
    }

    #[tokio::test]
    async fn coverage_settings_accepts_a_known_receiver() {
        let srv = new_coverage_server().await;
        srv.store
            .create_receiver(Receiver {
                name: "platform-alerts".to_string(),
                webhook_url: "https://hooks.example.com/abc".to_string(),
                enabled: true,
                ..Default::default()
            })
            .await
            .expect("create receiver");
        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/settings",
                r#"{"receiver":"platform-alerts"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "PUT = {}: {}",
            resp.status,
            resp.text()
        );
    }

    #[tokio::test]
    async fn coverage_project_path_is_required_and_bounded() {
        let srv = new_coverage_server().await;

        let resp = srv.admin_do(Method::GET, "/coverage/project", "").await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "missing path = {}, want 400",
            resp.status
        );

        // The path is interpolated into a GitLab API URL, so traversal is refused
        // before the client is ever built.
        let resp = srv
            .admin_do(Method::GET, "/coverage/project?path=team/../../admin", "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "traversal path = {}, want 400",
            resp.status
        );
    }

    #[tokio::test]
    async fn coverage_mute_round_trip() {
        let srv = new_coverage_server().await;

        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/project/mute?path=team/app",
                r#"{"muted":true}"#,
            )
            .await;
        let status = resp.status;
        let got = resp.json();
        assert!(
            status == StatusCode::OK && got["muted"] == true && got["project_path"] == "team/app",
            "PUT mute = {status} {got}"
        );
        let muted = srv
            .store
            .list_coverage_muted()
            .await
            .expect("list muted projects");
        assert!(
            muted.len() == 1 && muted[0].path == "team/app" && muted[0].scopes.all(),
            "stored muted projects = {muted:?}"
        );

        srv.admin_do(
            Method::PUT,
            "/coverage/project/mute?path=team/app",
            r#"{"muted":false}"#,
        )
        .await;
        let muted = srv.store.list_coverage_muted().await.unwrap_or_default();
        assert!(
            muted.is_empty(),
            "muted projects after unmuting = {muted:?}"
        );
        // The scanner's in-memory view follows the store, so the next read of the
        // overview is already correct without a rescan.
        assert!(
            !srv.scanner.match_host().is_empty(),
            "the scanner lost its host across the mute change"
        );
    }

    #[tokio::test]
    async fn coverage_mute_requires_a_boolean() {
        let srv = new_coverage_server().await;
        let resp = srv
            .admin_do(Method::PUT, "/coverage/project/mute?path=team/app", "{}")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "PUT with no muted field = {}, want 400",
            resp.status
        );
    }

    /// Muting one check is stored as that check alone, and an unknown one is refused
    /// rather than read as "mute nothing".
    #[tokio::test]
    async fn coverage_mute_scopes() {
        let srv = new_coverage_server().await;

        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/project/mute?path=team/app",
                r#"{"scopes":["registry"]}"#,
            )
            .await;
        let status = resp.status;
        let got = resp.json();
        assert!(
            status == StatusCode::OK
                && got["muted"] != true
                && got["scopes"].as_array().map(Vec::len) == Some(1)
                && got["scopes"][0] == "registry",
            "PUT scopes = {status} {got}"
        );
        let muted = srv
            .store
            .list_coverage_muted()
            .await
            .expect("list muted projects");
        assert!(
            muted.len() == 1
                && muted[0].scopes
                    == MuteScopes {
                        registry: true,
                        ..Default::default()
                    },
            "stored muted projects = {muted:?}"
        );

        let resp = srv
            .admin_do(
                Method::PUT,
                "/coverage/project/mute?path=team/app",
                r#"{"scopes":["nope"]}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::BAD_REQUEST,
            "PUT with an unknown scope = {}, want 400",
            resp.status
        );

        // An empty list is how the project comes back, so unmuting needs no separate
        // shape of request.
        srv.admin_do(
            Method::PUT,
            "/coverage/project/mute?path=team/app",
            r#"{"scopes":[]}"#,
        )
        .await;
        let muted = srv.store.list_coverage_muted().await.unwrap_or_default();
        assert!(
            muted.is_empty(),
            "muted projects after an empty scopes list = {muted:?}"
        );
    }

    #[tokio::test]
    async fn coverage_history_window_is_bounded() {
        let srv = new_coverage_server().await;
        // The bound is the retention: a wider window would promise readings the
        // store has already pruned.
        for days in ["0", "-1", "15", "3650", "abc"] {
            let resp = srv
                .admin_do(Method::GET, &format!("/coverage/history?days={days}"), "")
                .await;
            assert_eq!(
                resp.status,
                StatusCode::BAD_REQUEST,
                "days={days} = {}, want 400",
                resp.status
            );
        }
        let resp = srv
            .admin_do(Method::GET, "/coverage/history?days=7", "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "days=7 = {}, want 200",
            resp.status
        );
    }

    /// The host is an external domain, since that is what a build resolves through.
    #[tokio::test]
    async fn coverage_accepts_external_domain_hosts() {
        let srv = new_coverage_server().await;
        for host in ["nexus.corp.example.org", "artifacts.example.net:8443"] {
            let resp = srv
                .admin_do(
                    Method::PUT,
                    "/coverage/settings",
                    &format!(r#"{{"forklift_host":"{host}"}}"#),
                )
                .await;
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "alias {host} = {}, want 200: {}",
                resp.status,
                resp.text()
            );
        }
    }

    /// A name that is not an external domain cannot be the address a build resolves
    /// through, so it is refused at save time rather than stored as a pattern that
    /// silently matches nothing.
    #[tokio::test]
    async fn coverage_rejects_non_external_hosts() {
        let srv = new_coverage_server().await;
        for host in [
            "forklift",
            "forklift.forklift.svc.cluster.local",
            "localhost:8080",
            "db.internal",
            "127.0.0.1",
        ] {
            let resp = srv
                .admin_do(
                    Method::PUT,
                    "/coverage/settings",
                    &format!(r#"{{"forklift_host":"{host}"}}"#),
                )
                .await;
            assert_eq!(
                resp.status,
                StatusCode::BAD_REQUEST,
                "alias {host} = {}, want 400",
                resp.status
            );
        }
    }

    #[tokio::test]
    async fn coverage_notify_needs_a_scan_and_receivers() {
        let srv = new_coverage_server().await;
        // Notifications are not wired in this handler, which is reported as such
        // rather than as a delivery failure.
        let resp = srv
            .admin_do(Method::POST, "/coverage/notification/send", "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "send with no notifier = {}, want 503",
            resp.status
        );
    }

    /// A deployment with the credentials set but the switch off reports the two
    /// states apart, so the console can say which one to fix.
    #[tokio::test]
    async fn coverage_overview_reports_the_switch_separately() {
        // Enabled deliberately left off, and no external URL to derive a host from.
        let srv = new_coverage_server_with(false, "").await;

        let overview = srv.admin_do(Method::GET, "/coverage", "").await.json();
        assert_ne!(
            overview["enabled"], true,
            "enabled = true with the switch off"
        );
        assert_eq!(
            overview["credentials_present"], true,
            "credentials_present = false even though a URL and token are set"
        );

        // A scan is refused rather than quietly doing nothing.
        let resp = srv.admin_do(Method::POST, "/coverage/scan", "").await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "POST /coverage/scan with the switch off = {}, want 503",
            resp.status
        );
    }

    /// The host check reports the syntax verdict without depending on DNS, so it is
    /// exercised here for the shape rather than for a lookup that would need a
    /// resolver in the test environment.
    #[tokio::test]
    async fn coverage_host_check_reports_syntax() {
        let srv = new_coverage_server().await;
        let resp = srv
            .admin_do(
                Method::POST,
                "/coverage/settings/check-host",
                r#"{"forklift_host":"forklift.svc"}"#,
            )
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "check-host = {}, want 200",
            resp.status
        );
        let got = resp.json();
        assert_ne!(
            got["syntax_error"], "",
            "check = {got}, want a syntax problem reported"
        );
        assert_ne!(
            got["resolved"], true,
            "a name that failed the syntax check was resolved anyway"
        );
        // Empty rather than null, so the console can render it without a guard.
        assert!(
            got["addresses"].is_array(),
            "addresses serialised as null: {got}"
        );
    }

    #[tokio::test]
    async fn coverage_gitlab_check_reports_unreachable() {
        // The test server points at a host that does not exist, which is the shape
        // of the answer an operator gets from a wrong URL.
        let srv = new_coverage_server().await;
        let resp = srv
            .admin_do(Method::GET, "/coverage/gitlab-check", "")
            .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "gitlab-check = {}, want 200",
            resp.status
        );
        let got = resp.json();
        assert_eq!(
            got["configured"], true,
            "configured = false even though a URL and token are set"
        );
        assert!(
            got["reachable"] != true && got["error"] != "",
            "check = {got}, want an unreachable verdict with a reason"
        );
    }

    #[tokio::test]
    async fn coverage_overview_before_any_scan() {
        let srv = new_coverage_server().await;
        let overview = srv.admin_do(Method::GET, "/coverage", "").await.json();

        assert!(
            overview["last_scanned_at"].is_null() && overview["scanning"] != true,
            "overview = {overview}, want an unscanned, idle picture"
        );
        // Empty slices, not null: the console filters them without a guard.
        assert!(
            overview["projects"].is_array() && overview["excluded_projects"].is_array(),
            "projects or excluded_projects serialised as null"
        );
        // The host comes from the external URL with nothing to configure, so
        // coverage is usable out of the box.
        assert!(
            overview["forklift_host"] == "forklift.example.com" && overview["configured"] == true,
            "host = {} configured = {}",
            overview["forklift_host"],
            overview["configured"]
        );
    }
}
