//! The crawl: walking a GitLab instance project by project and turning each
//! one's files into a verdict.

use std::time::Instant;

use chrono::{Duration, Utc};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::coverage::adaptive::MAX_LIMIT;
use crate::coverage::evidence::{
    CI_FILE_RE, REGISTRY_FILE_RE, VENDOR_RE, Verdict, new_evidence_set,
};
use crate::coverage::gitlab::{GitLabClient, get_pages};
use crate::coverage::mute::verdict_for;
use crate::coverage::types::{
    EXCLUDE_MUTED, EXCLUDE_TOPIC_PREFIX, ExcludedProject, PHASE_LISTING, PHASE_SCANNING, Progress,
    Project, STATE_APPLIED, STATE_ERROR, STATE_PARTIAL, Settings, Snapshot,
};
use crate::coverage::views::{non_nil, parse_gitlab_time, path_escape, split_path};
use crate::coverage::{Error, Res, Scanner};

/// rawProject is the subset of GitLab's project representation the scan reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RawProject {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) name: String,
    #[serde(default, rename = "path_with_namespace")]
    pub(crate) path_with_namespace: String,
    #[serde(default)]
    pub(crate) web_url: String,
    #[serde(default)]
    pub(crate) default_branch: String,
    #[serde(default)]
    pub(crate) topics: Vec<String>,
    #[serde(default)]
    pub(crate) last_activity_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawBranch {
    #[serde(default)]
    name: String,
    #[serde(default)]
    commit: RawBranchCommit,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawBranchCommit {
    #[serde(default)]
    committed_date: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawTreeEntry {
    #[serde(default, rename = "type")]
    entry_type: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawBlobHit {
    #[serde(default)]
    path: String,
    #[serde(default)]
    data: String,
}

impl Scanner {
    /// Scan walks every in-scope project and replaces the coverage picture.
    ///
    /// `triggered_by` names whoever asked: a username for a manual run, or
    /// [`crate::coverage::TRIGGER_SCHEDULE`] / [`crate::coverage::TRIGGER_STARTUP`].
    /// It is recorded only once the scan succeeds, so a failed run cannot
    /// rewrite the attribution of the result still on screen.
    pub async fn scan(&self, triggered_by: &str) -> Res<()> {
        self.scan_with_cancel(triggered_by, CancellationToken::new())
            .await
    }

    /// Dropping the future stops the crawl too; the token exists so a caller holding only an
    /// `Arc<Scanner>` can stop one it did not spawn.
    pub async fn scan_with_cancel(&self, triggered_by: &str, cancel: CancellationToken) -> Res<()> {
        if !self.enabled() {
            if self.credentials_present() {
                return Err(Error::Msg(
                    "coverage scanning is turned off; enable it to run a scan".to_string(),
                ));
            }
            return Err(Error::Msg(
                "coverage scanning is not configured: set the GitLab URL and token".to_string(),
            ));
        }
        let _guard = match self.scan_mu.try_lock() {
            Ok(g) => g,
            Err(_) => return Err(Error::Msg("a scan is already in progress".to_string())),
        };

        // Settings can change between scans, so resolve them up front rather
        // than trusting whatever the last refresh cached.
        self.refresh_settings().await?;
        self.refresh_muted().await?;
        let cfg = self.settings();
        let host = self.match_host();
        if host.is_empty() {
            return Err(Error::Msg(
                "no forklift host is known: set FORKLIFT_EXTERNAL_URL, or save one in the coverage settings"
                    .to_string(),
            ));
        }

        let started = Instant::now();
        let started_at = Utc::now();
        {
            let mut state = self.state.write();
            state.scanning = true;
            // Published before the first API call so the console can show the
            // scan is alive while the project list is still paging in.
            state.progress = Some(Progress {
                phase: PHASE_LISTING.to_string(),
                started_at,
                ..Progress::default()
            });
        }

        let err = self
            .run_scan(&cfg, &host, triggered_by, started, &cancel)
            .await;

        {
            let mut state = self.state.write();
            state.scanning = false;
            state.progress = None;
            match &err {
                Err(e) => {
                    state.last_scan_error = e.to_string();
                    state.scans_failed += 1;
                }
                Ok(()) => state.scans_succeeded += 1,
            }
        }

        if let Err(e) = &err {
            tracing::error!(err = %e, "coverage: scan failed");
            // A half-finished list is not a result. Fall back to the last
            // completed scan rather than leaving partial rows on the page.
            if let Err(restore_err) = self.restore_last_result().await {
                tracing::warn!(err = %restore_err, "coverage: restoring the previous result failed");
            }
        }
        err
    }

    async fn run_scan(
        &self,
        cfg: &Settings,
        host: &str,
        triggered_by: &str,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Res<()> {
        let client = self.new_client_with(cancel.clone());
        let (candidates, excluded) = self.list_projects(&client, cfg).await?;

        {
            let mut state = self.state.write();
            state.excluded_projects = excluded.clone();
            if let Some(progress) = state.progress.as_mut() {
                progress.phase = PHASE_SCANNING.to_string();
                progress.total = candidates.len() as i64;
                progress.excluded = excluded.len() as i64;
            }
            // Results are published as each project finishes, so the console
            // fills in during the scan instead of staying on the previous run
            // for minutes.
            state.projects = Vec::with_capacity(candidates.len());
        }

        tracing::info!(
            projects = candidates.len(),
            excluded = excluded.len(),
            "coverage: scanning"
        );

        // The worker pool is deliberately not a setting. Workers spend most of their time
        // blocked on a slot, so an idle one costs a parked future and nothing else.
        let workers = (MAX_LIMIT as usize).min(candidates.len());

        let cursor = Mutex::new(0usize);
        let mut tasks = Vec::with_capacity(workers);
        for _ in 0..workers {
            tasks.push(async {
                loop {
                    let index = {
                        let mut c = cursor.lock();
                        let index = *c;
                        *c += 1;
                        index
                    };
                    if index >= candidates.len() {
                        return;
                    }
                    if cancel.is_cancelled() {
                        return;
                    }
                    let result = self
                        .scan_project(&client, cfg, host, &candidates[index])
                        .await;
                    self.record_project(result);
                }
            });
        }
        futures_util::future::join_all(tasks).await;
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let finished_at = Utc::now();
        let duration_ms = started.elapsed().as_millis() as i64;
        {
            let mut state = self.state.write();
            state.last_scanned_at = Some(finished_at);
            state.last_duration_ms = duration_ms;
            state.last_triggered_by = triggered_by.to_string();
            state.last_scan_error = String::new();
        }

        self.persist().await?;

        let summary = self.summary();
        if let Err(e) = self
            .store
            .add_coverage_snapshot(Snapshot {
                target: summary.target,
                applied: summary.applied,
                partial: summary.partial,
                not_applied: summary.not_applied,
                skipped: summary.skipped,
                percent: summary.percent,
                scanned_at: finished_at,
            })
            .await
        {
            tracing::warn!(err = %e, "coverage: recording the history snapshot failed");
        }

        // The concurrency the crawl settled on is logged because it is no longer
        // configured: this is the only place an operator can see what rate the
        // instance turned out to tolerate.
        let (limit, peak) = client.concurrency_stats();
        {
            let mut state = self.state.write();
            state.last_concurrency = limit;
            state.peak_concurrency = peak;
        }
        tracing::info!(
            target = summary.target,
            applied = summary.applied,
            percent = summary.percent,
            partial = summary.partial,
            not_applied = summary.not_applied,
            errored = summary.errored,
            no_ci = summary.skipped,
            concurrency = limit,
            peak_concurrency = peak,
            duration = %round_to_seconds(duration_ms),
            "coverage: scan complete"
        );
        Ok(())
    }

    /// recordProject appends a finished verdict and advances the rolling counts.
    fn record_project(&self, p: Project) {
        let mut state = self.state.write();
        let (skipped, applied) = (p.skipped, p.applied);
        state.projects.push(p);
        let progress = match state.progress.as_mut() {
            Some(progress) => progress,
            None => return,
        };
        progress.done += 1;
        if skipped {
            progress.skipped += 1;
        } else if applied == STATE_APPLIED {
            progress.applied += 1;
        } else if applied == STATE_PARTIAL {
            progress.partial += 1;
        } else if applied == STATE_ERROR {
            progress.errored += 1;
        } else {
            progress.not_applied += 1;
        }
    }

    /// listProjects pages in the project list and splits it into candidates and
    /// the projects excluded before they were scanned.
    ///
    /// What is in scope is decided by the access token, not by a setting: a
    /// group access token sees that group and its subgroups and nothing else,
    /// which is the same narrowing a group name in a form would express,
    /// enforced by GitLab rather than by forklift asking politely.
    async fn list_projects(
        &self,
        client: &GitLabClient,
        cfg: &Settings,
    ) -> Res<(Vec<RawProject>, Vec<ExcludedProject>)> {
        let all: Vec<RawProject> = get_pages(
            client,
            "projects?per_page=100&archived=false&membership=true&simple=false",
        )
        .await
        .map_err(|e| match e {
            Error::NotFound | Error::NotFoundAt { .. } => e,
            other => Error::Msg(format!("listing projects: {other}")),
        })?;

        let mut candidates: Vec<RawProject> = Vec::with_capacity(all.len());
        let mut excluded: Vec<ExcludedProject> = Vec::new();
        for raw in all {
            // An empty repository has no default branch and nothing to scan.
            if raw.default_branch.is_empty() {
                continue;
            }
            let reason = self.exclude_reason(cfg, &raw);
            if reason.is_empty() {
                candidates.push(raw);
                continue;
            }
            let (group, name) = split_path(&raw.path_with_namespace);
            excluded.push(ExcludedProject {
                id: raw.id,
                path: raw.path_with_namespace,
                group,
                name,
                web_url: raw.web_url,
                default_branch: raw.default_branch,
                topics: non_nil(raw.topics),
                reason,
                last_activity_at: parse_gitlab_time(&raw.last_activity_at),
            });
        }
        Ok((candidates, excluded))
    }

    /// excludeReason returns why the project is out of scope, or "" when it
    /// counts.
    fn exclude_reason(&self, cfg: &Settings, raw: &RawProject) -> String {
        // Only a project with every check muted is dropped before the scan.
        // Muting one half leaves it in: it still has a verdict, just one that
        // stops asking for that half.
        if self.muted_scopes(&raw.path_with_namespace).all() {
            return EXCLUDE_MUTED.to_string();
        }
        for topic in &raw.topics {
            for want in &cfg.exclude_topics {
                if topic.trim().eq_ignore_ascii_case(want.trim()) {
                    return format!("{EXCLUDE_TOPIC_PREFIX}{topic}");
                }
            }
        }
        String::new()
    }

    /// scanProject produces one project's verdict. Every GitLab failure becomes
    /// an "error" verdict rather than aborting the scan: one unreadable project
    /// must not cost the whole run.
    async fn scan_project(
        &self,
        client: &GitLabClient,
        cfg: &Settings,
        host: &str,
        raw: &RawProject,
    ) -> Project {
        let (group, name) = split_path(&raw.path_with_namespace);
        let muted = self.muted_scopes(&raw.path_with_namespace);
        let base = Project {
            id: raw.id,
            path: raw.path_with_namespace.clone(),
            group,
            name,
            web_url: raw.web_url.clone(),
            default_branch: raw.default_branch.clone(),
            topics: non_nil(raw.topics.clone()),
            applied: verdict_for(false, false, muted),
            muted_scopes: muted.list(),
            evidence: Vec::new(),
            last_activity_at: parse_gitlab_time(&raw.last_activity_at),
            ..Project::default()
        };
        let fail = |stage: &str, branch: &str, err: Error| -> Project {
            Project {
                applied: STATE_ERROR,
                branch: branch.to_string(),
                note: format!("{stage}: {err}"),
                ..base.clone()
            }
        };
        let applied = |v: Verdict, branch: &str| -> Project {
            let on_default = branch == raw.default_branch;
            Project {
                // A muted check is neither required nor credited, so the verdict
                // is derived from what is still being asked of the project.
                applied: verdict_for(v.ci_wired, v.registry_pinned, muted),
                branch: branch.to_string(),
                on_default: Some(on_default),
                format: v.format,
                ci_wired: v.ci_wired,
                registry_pinned: v.registry_pinned,
                evidence: v.evidence,
                ..base.clone()
            }
        };

        // Fast path. The blob-search endpoint answers for the default branch in
        // one call, but it runs on a much lower rate limit than the plain API,
        // so it stays opt-in.
        if cfg.use_search {
            match self.search_default_branch(client, host, raw.id).await {
                Err(e) => return fail("search_api_failed", "", e),
                Ok(v) => {
                    if v.hit {
                        return applied(v, &raw.default_branch);
                    }
                }
            }
        }

        let branches: Vec<RawBranch> = match get_pages(
            client,
            &format!("projects/{}/repository/branches?per_page=100", raw.id),
        )
        .await
        {
            Ok(b) => b,
            Err(e) if e.is_not_found() => {
                return Project {
                    skipped: true,
                    ..base
                };
            }
            Err(e) => return fail("branches_api_failed", "", e),
        };

        let (order, other_count) = branch_order(&branches, &raw.default_branch, cfg.since_days);

        let mut any_ci = false;
        let mut scanned_others: i64 = 0;
        for branch in &order {
            // The default branch is always checked; the cap applies to the rest.
            if branch != &raw.default_branch {
                if scanned_others >= cfg.max_branches {
                    break;
                }
                scanned_others += 1;
            } else if cfg.use_search {
                continue; // already covered by the fast path
            }

            let v = match self.scan_branch(client, host, raw.id, branch).await {
                Ok(v) => v,
                Err(e) => return fail("branch_api_failed", branch, e),
            };
            if v.has_ci {
                any_ci = true;
            }
            if !v.hit {
                continue;
            }
            // Wiring found. The remaining branches add nothing to the verdict.
            return applied(v, branch);
        }

        // No CI file anywhere means the project was never an integration target.
        if !any_ci {
            return Project {
                skipped: true,
                ..base
            };
        }
        let mut p = base;
        if other_count > cfg.max_branches {
            p.note = format!("branches_truncated:{scanned_others}/{other_count}");
        }
        p
    }

    /// searchDefaultBranch is the single blob-search call. It only covers the
    /// default branch.
    async fn search_default_branch(
        &self,
        client: &GitLabClient,
        host: &str,
        project_id: i64,
    ) -> Res<Verdict> {
        let hits: Vec<RawBlobHit> = match client
            .get_json(&format!(
                "projects/{project_id}/search?scope=blobs&search=forklift&per_page=100"
            ))
            .await
        {
            Ok(hits) => hits,
            Err(e) if e.is_not_found() => return Ok(Verdict::default()),
            Err(e) => return Err(e),
        };

        let mut found = new_evidence_set(host);
        for hit in &hits {
            // A README that merely mentions forklift is not evidence. Only CI
            // and registry files count, the same as the per-branch scan.
            if !CI_FILE_RE.is_match(&hit.path) && !REGISTRY_FILE_RE.is_match(&hit.path) {
                continue;
            }
            if VENDOR_RE.is_match(&hit.path) {
                continue;
            }
            found.add(&hit.path, &hit.data);
        }
        if found.empty() {
            return Ok(Verdict::default());
        }
        let mut v = found.verdict();
        v.has_ci = true;
        Ok(v)
    }

    /// scanBranch walks the tree of one ref and reads every candidate file.
    async fn scan_branch(
        &self,
        client: &GitLabClient,
        host: &str,
        project_id: i64,
        r#ref: &str,
    ) -> Res<Verdict> {
        let tree: Vec<RawTreeEntry> = match get_pages(
            client,
            &format!(
                "projects/{project_id}/repository/tree?ref={}&recursive=true&per_page=100",
                path_escape(r#ref)
            ),
        )
        .await
        {
            Ok(tree) => tree,
            Err(e) if e.is_not_found() => return Ok(Verdict::default()),
            Err(e) => return Err(e),
        };

        let mut has_ci = false;
        let mut candidates: Vec<String> = Vec::with_capacity(8);
        for entry in &tree {
            if entry.entry_type != "blob" || VENDOR_RE.is_match(&entry.path) {
                continue;
            }
            let is_ci = CI_FILE_RE.is_match(&entry.path);
            if is_ci {
                has_ci = true;
            }
            if is_ci || REGISTRY_FILE_RE.is_match(&entry.path) {
                candidates.push(entry.path.clone());
            }
        }
        if candidates.is_empty() {
            return Ok(Verdict {
                has_ci,
                ..Verdict::default()
            });
        }

        let mut found = new_evidence_set(host);
        for file_path in &candidates {
            let body = match client
                .get_text(&format!(
                    "projects/{project_id}/repository/files/{}/raw?ref={}",
                    path_escape(file_path),
                    path_escape(r#ref)
                ))
                .await
            {
                Ok(body) => body,
                Err(e) if e.is_not_found() => continue,
                Err(e) => return Err(e),
            };
            found.add(file_path, &body);
        }
        if found.empty() {
            return Ok(Verdict {
                has_ci,
                ..Verdict::default()
            });
        }
        let mut v = found.verdict();
        v.has_ci = has_ci;
        Ok(v)
    }
}

/// branchOrder puts the default branch first, then the other branches active
/// within `since_days`, most recently committed first. It also reports how many
/// other branches were in scope, so a truncated scan can say so.
fn branch_order(
    branches: &[RawBranch],
    default_branch: &str,
    since_days: i64,
) -> (Vec<String>, i64) {
    let cutoff = Utc::now() - Duration::days(since_days);
    let mut others: Vec<&RawBranch> = Vec::with_capacity(branches.len());
    let mut has_default = false;
    for b in branches {
        if b.name == default_branch {
            has_default = true;
            continue;
        }
        if parse_gitlab_time(&b.commit.committed_date) < cutoff {
            continue;
        }
        others.push(b);
    }
    others.sort_by(|a, b| {
        parse_gitlab_time(&b.commit.committed_date)
            .cmp(&parse_gitlab_time(&a.commit.committed_date))
    });

    let mut order: Vec<String> = Vec::with_capacity(others.len() + 1);
    if has_default {
        order.push(default_branch.to_string());
    }
    for b in &others {
        order.push(b.name.clone());
    }
    (order, others.len() as i64)
}

fn round_to_seconds(ms: i64) -> String {
    let total = (ms as f64 / 1000.0).round() as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}s"));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parking_lot::Mutex;
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use crate::coverage::types::default_settings;
    use crate::coverage::views::url_query_unescape;
    use crate::coverage::{
        MuteScopes, MutedProject, Project, Res, Result as ScanResult, Scanner, ScannerOptions,
        Settings, Snapshot, Store, types::STATE_APPLIED, types::STATE_ERROR,
        types::STATE_NOT_APPLIED, types::STATE_PARTIAL,
    };

    pub(crate) const HOST: &str = "forklift.example.com";

    /// reqwest is built on `rustls-no-provider`, so a client cannot be constructed
    /// until a crypto provider is installed. The server module does this once at
    /// startup; the tests do it here.
    pub(crate) fn install_crypto_provider() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
        });
    }

    // ---------------------------------------------------------------------------
    // memStore: an in-memory Store, so the scanner can be exercised without a
    // database.
    // ---------------------------------------------------------------------------

    #[derive(Default)]
    struct MemStoreInner {
        settings: Option<Settings>,
        result: Option<ScanResult>,
        snapshots: Vec<Snapshot>,
        muted: HashMap<String, MuteScopes>,
    }

    pub(crate) struct MemStore {
        inner: Mutex<MemStoreInner>,
    }

    pub(crate) fn new_mem_store(s: Settings) -> Arc<MemStore> {
        Arc::new(MemStore {
            inner: Mutex::new(MemStoreInner {
                settings: Some(s),
                ..MemStoreInner::default()
            }),
        })
    }

    impl MemStore {
        pub(crate) fn snapshot_count(&self) -> usize {
            self.inner.lock().snapshots.len()
        }
    }

    #[async_trait::async_trait]
    impl Store for MemStore {
        async fn read_coverage_settings(&self) -> Res<Option<Settings>> {
            Ok(self.inner.lock().settings.clone())
        }

        async fn read_coverage_result(&self) -> Res<Option<ScanResult>> {
            Ok(self.inner.lock().result.clone())
        }

        async fn write_coverage_result(&self, r: ScanResult) -> Res<()> {
            self.inner.lock().result = Some(r);
            Ok(())
        }

        async fn add_coverage_snapshot(&self, s: Snapshot) -> Res<()> {
            self.inner.lock().snapshots.push(s);
            Ok(())
        }

        async fn list_coverage_muted(&self) -> Res<Vec<MutedProject>> {
            Ok(self
                .inner
                .lock()
                .muted
                .iter()
                .map(|(path, scopes)| MutedProject {
                    path: path.clone(),
                    scopes: *scopes,
                })
                .collect())
        }

        async fn add_coverage_muted(&self, path: &str, _by: &str, scopes: MuteScopes) -> Res<()> {
            self.inner.lock().muted.insert(path.to_string(), scopes);
            Ok(())
        }

        async fn remove_coverage_muted(&self, path: &str) -> Res<()> {
            self.inner.lock().muted.remove(path);
            Ok(())
        }
    }

    // ---------------------------------------------------------------------------
    // fakeGitLab: the handful of endpoints the scanner calls.
    // ---------------------------------------------------------------------------

    /// fakeProject is one project the GitLab stub serves.
    #[derive(Clone, Default)]
    pub(crate) struct FakeProject {
        pub(crate) id: i64,
        pub(crate) path: String,
        pub(crate) topics: Vec<String>,
        pub(crate) branches: Vec<String>,
        /// files maps "<ref>:<path>" to the file body. A project with no file whose
        /// path matches the CI pattern reads as having no CI at all.
        pub(crate) files: HashMap<String, String>,
        /// status, when non-zero, is returned for every request about this project,
        /// which is how the error and not-found paths are exercised.
        pub(crate) status: u16,
    }

    pub(crate) fn fake_project(
        id: i64,
        path: &str,
        branches: &[&str],
        files: &[(&str, String)],
    ) -> FakeProject {
        FakeProject {
            id,
            path: path.to_string(),
            topics: Vec::new(),
            branches: branches.iter().map(|b| b.to_string()).collect(),
            files: files
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            status: 0,
        }
    }

    struct FakeGitLab {
        projects: Vec<FakeProject>,
        /// The scanner addresses a project by its numeric id during a scan and by
        /// its URL-encoded path when it has to look one up, so the stub answers to
        /// both.
        by_id: HashMap<String, usize>,
    }

    fn project_json(p: &FakeProject, topics: serde_json::Value) -> serde_json::Value {
        let name = &p.path[p.path.rfind('/').map(|i| i + 1).unwrap_or(0)..];
        json!({
            "id": p.id,
            "name": name,
            "path_with_namespace": p.path,
            "web_url": format!("https://gitlab.example.com/{}", p.path),
            "default_branch": "main",
            "topics": topics,
            "last_activity_at": "2026-08-01T00:00:00.000Z",
        })
    }

    fn plain_error(status: u16, msg: &str) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .insert_header("Content-Type", "text/plain; charset=utf-8")
            .set_body_string(format!("{msg}\n"))
    }

    fn json_list(v: serde_json::Value) -> ResponseTemplate {
        // No x-next-page header: every stub response is a single page.
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_json(v)
    }

    impl Respond for FakeGitLab {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let raw_path = req.url.path().to_string();
            let path = percent_encoding::percent_decode_str(&raw_path)
                .decode_utf8_lossy()
                .into_owned();

            if path == "/api/v4/projects" {
                let out: Vec<serde_json::Value> = self
                    .projects
                    .iter()
                    .map(|p| project_json(p, json!(p.topics)))
                    .collect();
                return json_list(json!(out));
            }
            let rest = match path.strip_prefix("/api/v4/projects/") {
                Some(rest) => rest,
                None => return plain_error(404, "not found"),
            };
            let rest = match url_query_unescape(rest) {
                Ok(rest) => rest,
                Err(_) => return plain_error(400, "bad path"),
            };

            let (id, tail) = match self.by_id.get(rest.as_str()) {
                Some(&i) if self.projects[i].id != 0 => (rest.clone(), String::new()),
                _ => match rest.split_once('/') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (rest.clone(), String::new()),
                },
            };
            let p = match self.by_id.get(id.as_str()) {
                Some(&i) => &self.projects[i],
                None => return plain_error(404, "no such project"),
            };
            if p.status != 0 {
                return plain_error(p.status, "boom");
            }
            let r#ref = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "ref")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();

            if tail.is_empty() {
                // The project itself, which is how a path not covered by the last
                // scan is resolved for the detail view.
                return ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_json(project_json(p, json!([] as [&str; 0])));
            }
            if tail == "repository/branches" {
                let out: Vec<serde_json::Value> = p
                    .branches
                    .iter()
                    .map(|b| {
                        json!({
                            "name": b,
                            "commit": {"committed_date": "2026-08-01T00:00:00.000Z"},
                        })
                    })
                    .collect();
                return json_list(json!(out));
            }
            if tail == "repository/tree" {
                let mut out: Vec<serde_json::Value> = Vec::new();
                for key in p.files.keys() {
                    let (file_ref, file_path) = match key.split_once(':') {
                        Some(v) => v,
                        None => (key.as_str(), ""),
                    };
                    if file_ref != r#ref {
                        continue;
                    }
                    out.push(json!({"type": "blob", "path": file_path}));
                }
                return json_list(json!(out));
            }
            if let Some(after) = tail.strip_prefix("repository/files/") {
                let file_path = after.strip_suffix("/raw").unwrap_or(after);
                let decoded = match url_query_unescape(file_path) {
                    Ok(decoded) => decoded,
                    Err(_) => return plain_error(400, "bad path"),
                };
                return match p.files.get(&format!("{}:{}", r#ref, decoded)) {
                    Some(body) => ResponseTemplate::new(200).set_body_string(body.clone()),
                    None => plain_error(404, "no such file"),
                };
            }
            if tail == "repository/commits" {
                return json_list(json!([{
                    "short_id": "abc1234",
                    "title": "wire forklift",
                    "author_name": "dev",
                    "committed_date": "2026-08-01T00:00:00.000Z",
                    "web_url": format!("https://gitlab.example.com/{}/-/commit/abc1234", p.path),
                }]));
            }
            plain_error(404, &format!("unhandled {tail}"))
        }
    }

    pub(crate) async fn fake_gitlab(projects: Vec<FakeProject>) -> MockServer {
        install_crypto_provider();
        let mut by_id = HashMap::new();
        for (i, p) in projects.iter().enumerate() {
            by_id.insert(p.id.to_string(), i);
            by_id.insert(p.path.clone(), i);
        }
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(FakeGitLab { projects, by_id })
            .mount(&srv)
            .await;
        srv
    }

    pub(crate) fn new_test_scanner(
        store: Arc<dyn Store>,
        srv: &MockServer,
        mutate: Option<fn(&mut Settings)>,
    ) -> Arc<Scanner> {
        install_crypto_provider();
        let s = Scanner::new(ScannerOptions {
            store,
            enabled: true,
            gitlab_url: srv.uri(),
            gitlab_token: "test-token".to_string(),
            forklift_host: format!("https://{HOST}"),
        });
        if let Some(mutate) = mutate {
            let mut cfg = s.settings();
            mutate(&mut cfg);
            s.state.write().settings = cfg;
        }
        s
    }

    /// withHost is the default settings; the host itself is derived by the scanner
    /// from the external URL rather than stored, so there is nothing to set here.
    pub(crate) fn with_host(_: &str) -> Settings {
        default_settings()
    }

    fn by_path(projects: Vec<Project>) -> HashMap<String, Project> {
        projects.into_iter().map(|p| (p.path.clone(), p)).collect()
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn scan_verdicts() {
        let projects = vec![
            // CI and a registry file both point at forklift: fully applied.
            fake_project(
                1,
                "payments/checkout-api",
                &["main"],
                &[
                    (
                        "main:.gitlab-ci.yml",
                        format!("image: {HOST}/docker/base:1\n"),
                    ),
                    (
                        "main:.npmrc",
                        format!("registry=https://{HOST}/npm/npmjs/\n"),
                    ),
                ],
            ),
            // Only the CI file does: half the wiring, so partial.
            fake_project(
                2,
                "platform/worker",
                &["main"],
                &[
                    (
                        "main:.gitlab-ci.yml",
                        format!("script: curl https://{HOST}/maven/central/\n"),
                    ),
                    ("main:pom.xml", "<project/>\n".to_string()),
                ],
            ),
            // Has CI, but nothing references forklift: a real migration target.
            fake_project(
                3,
                "data/etl",
                &["main"],
                &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
            ),
            // No CI file at all, so it was never an integration candidate.
            fake_project(
                4,
                "docs/handbook",
                &["main"],
                &[("main:README.md", format!("we use {HOST}\n"))],
            ),
            // Excluded by topic, before any request is made about it.
            FakeProject {
                topics: vec!["forklift.excluded".to_string()],
                ..fake_project(
                    5,
                    "legacy/thing",
                    &["main"],
                    &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
                )
            },
            // Every request 500s, which becomes an error verdict rather than
            // failing the whole scan.
            FakeProject {
                status: 500,
                ..fake_project(6, "broken/project", &["main"], &[])
            },
        ];
        let srv = fake_gitlab(projects).await;
        let store = new_mem_store(default_settings());
        let scanner = new_test_scanner(store, &srv, None);

        scanner.scan("tester").await.expect("Scan");

        let by = by_path(scanner.overview().projects);

        let got = &by["payments/checkout-api"];
        assert!(
            got.applied == STATE_APPLIED && got.ci_wired && got.registry_pinned,
            "checkout-api = {got:?}, want applied with both halves wired"
        );
        assert_eq!(
            got.format, "docker/npm",
            "checkout-api format, want the formats read out of the forklift URLs"
        );
        let got = &by["platform/worker"];
        assert!(
            got.applied == STATE_PARTIAL && got.ci_wired && !got.registry_pinned,
            "worker = {got:?}, want partial with only CI wired"
        );
        let got = &by["data/etl"];
        assert!(
            got.applied == STATE_NOT_APPLIED && !got.skipped,
            "etl = {got:?}, want not applied and in scope"
        );
        assert!(
            by["docs/handbook"].skipped,
            "handbook, want skipped: a README mention is not evidence"
        );
        assert_eq!(
            by["broken/project"].applied, STATE_ERROR,
            "broken, want an error verdict"
        );
        assert!(
            !by.contains_key("legacy/thing"),
            "a project excluded by topic was scanned anyway"
        );

        let summary = scanner.summary();
        // Target counts only the in-scope projects that have CI: applied, partial,
        // not applied and errored. The no-CI and excluded ones sit outside it.
        assert_eq!(
            (
                summary.target,
                summary.applied,
                summary.partial,
                summary.not_applied,
                summary.errored,
                summary.skipped,
                summary.excluded
            ),
            (4, 1, 1, 1, 1, 1, 1),
            "summary = {summary:?}"
        );
        assert_eq!(
            summary.percent, 25,
            "percent, want 25 (1 applied of 4 target)"
        );
    }

    #[tokio::test]
    async fn scan_token_reference_counts_as_evidence() {
        // A job that authenticates with the forklift token without naming the host
        // is still wired, and the format falls back to the filenames that matched.
        let srv = fake_gitlab(vec![fake_project(
        1,
        "team/app",
        &["main"],
        &[
            (
                "main:.gitlab-ci.yml",
                "script:\n  - mvn -s settings.xml deploy\n  - echo \"$FORKLIFT_DEPLOY_TOKEN\" > ~/.netrc\n"
                    .to_string(),
            ),
            (
                "main:settings.xml",
                "<settings><server><password>${FORKLIFT_DEPLOY_TOKEN}</password></server></settings>"
                    .to_string(),
            ),
        ],
    )])
    .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store, &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let got = scanner.overview().projects[0].clone();
        assert_eq!(got.applied, STATE_APPLIED);
        assert_eq!(got.format, "maven", "want maven guessed from the filenames");
    }

    #[tokio::test]
    async fn scan_ignores_vendored_files() {
        // A committed dependency tree mentions the host without the project having
        // chosen it, so it must not count as wiring.
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main"],
            &[
                ("main:.gitlab-ci.yml", "script: echo hi\n".to_string()),
                (
                    "main:node_modules/dep/package.json",
                    format!(r#"{{"registry":"https://{HOST}/npm/npmjs/"}}"#),
                ),
            ],
        )])
        .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store, &srv, None);
        scanner.scan("tester").await.expect("Scan");
        assert_eq!(
            scanner.overview().projects[0].applied,
            STATE_NOT_APPLIED,
            "a vendored file is not the project's own wiring"
        );
    }

    #[tokio::test]
    async fn scan_finds_wiring_on_a_non_default_branch() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main", "feature/forklift"],
            &[
                ("main:.gitlab-ci.yml", "script: echo hi\n".to_string()),
                (
                    "feature/forklift:.gitlab-ci.yml",
                    format!("image: {HOST}/docker/base:1\n"),
                ),
                (
                    "feature/forklift:.npmrc",
                    format!("registry=https://{HOST}/npm/npmjs/\n"),
                ),
            ],
        )])
        .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store, &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let got = scanner.overview().projects[0].clone();
        assert_eq!(
            got.branch, "feature/forklift",
            "want the branch the wiring is actually on"
        );
        assert_eq!(
            got.on_default,
            Some(false),
            "want false so the console can flag it"
        );
    }

    #[tokio::test]
    async fn scan_refuses_without_a_forklift_host() {
        // No external URL and no alias means nothing to match, which would report
        // every project as not applied rather than reporting a misconfiguration.
        let srv = fake_gitlab(Vec::new()).await;
        let scanner = Scanner::new(ScannerOptions {
            store: new_mem_store(default_settings()),
            enabled: true,
            gitlab_url: srv.uri(),
            gitlab_token: "test-token".to_string(),
            forklift_host: String::new(),
        });
        assert!(
            !scanner.configured(),
            "configured() is true with no host to match against"
        );
        assert!(
            scanner.scan("tester").await.is_err(),
            "Scan succeeded with no forklift host"
        );
    }

    /// A saved host overrides the one derived from the external URL, which is how a
    /// deployment whose builds resolve through a different domain is measured.
    #[tokio::test]
    async fn scan_matches_a_saved_host() {
        const INTERNAL: &str = "artifacts.example.org";
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main"],
            &[
                (
                    "main:.gitlab-ci.yml",
                    format!("image: {INTERNAL}/docker/base:1\n"),
                ),
                (
                    "main:.npmrc",
                    format!("registry=https://{INTERNAL}/npm/npmjs/\n"),
                ),
            ],
        )])
        .await;
        let mut cfg = default_settings();
        cfg.forklift_host = INTERNAL.to_string();
        let scanner = new_test_scanner(new_mem_store(cfg), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let got = scanner.overview().projects[0].clone();
        assert_eq!(
            got.applied, STATE_APPLIED,
            "want the alias to count as forklift"
        );
        assert_eq!(
            got.format, "docker/npm",
            "want the formats read out of the alias URLs"
        );
    }

    #[tokio::test]
    async fn scan_requires_gitlab() {
        let scanner = Scanner::new(ScannerOptions {
            store: new_mem_store(with_host(HOST)),
            enabled: true,
            gitlab_url: String::new(),
            gitlab_token: String::new(),
            forklift_host: String::new(),
        });
        assert!(
            !scanner.enabled(),
            "enabled() is true with no GitLab URL or token"
        );
        assert!(
            !scanner.credentials_present(),
            "credentials_present() is true with no GitLab URL or token"
        );
        assert!(
            scanner.scan("tester").await.is_err(),
            "Scan succeeded with no GitLab configured"
        );
    }

    /// The switch is separate from the credentials on purpose: a GitLab token that
    /// happens to be in the environment must not start a crawl against that
    /// instance because forklift gained the ability to.
    #[tokio::test]
    async fn scan_requires_the_explicit_switch() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main"],
            &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
        )])
        .await;
        let scanner = Scanner::new(ScannerOptions {
            store: new_mem_store(with_host(HOST)),
            enabled: false,
            gitlab_url: srv.uri(),
            gitlab_token: "test-token".to_string(),
            forklift_host: format!("https://{HOST}"),
        });
        assert!(!scanner.enabled(), "enabled() is true with the switch off");
        // The credentials are still reported as present, which is what lets the
        // console say "turned off" rather than "not set up".
        assert!(
            scanner.credentials_present(),
            "credentials_present() is false even though a URL and token are set"
        );
        assert!(
            scanner.scan("tester").await.is_err(),
            "Scan ran with the switch off"
        );
        let got = scanner.overview();
        assert!(
            !got.enabled && got.credentials_present,
            "overview = enabled {}, credentials_present {}",
            got.enabled,
            got.credentials_present
        );
    }

    #[tokio::test]
    async fn scan_persists_result_and_snapshot() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main"],
            &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
        )])
        .await;
        let store = new_mem_store(with_host(HOST));
        let scanner = new_test_scanner(store.clone(), &srv, None);
        scanner.scan("alice").await.expect("Scan");

        let stored = store
            .read_coverage_result()
            .await
            .expect("read_coverage_result")
            .expect("a stored result");
        assert_eq!(stored.triggered_by, "alice");
        assert_eq!(stored.projects.len(), 1);
        assert_eq!(
            store.snapshot_count(),
            1,
            "want one history snapshot per completed scan"
        );

        // A fresh scanner over the same store shows the previous picture, which is
        // what keeps a restart from emptying the page.
        let restored = new_test_scanner(store, &srv, None);
        restored.load().await.expect("Load");
        let got = restored.overview();
        assert!(got.last_scanned_at.is_some());
        assert_eq!(got.projects.len(), 1, "want the stored scan");
    }

    #[tokio::test]
    async fn group_coverage_is_worst_first() {
        let wired: Vec<(&str, String)> = vec![
            (
                "main:.gitlab-ci.yml",
                format!("image: {HOST}/docker/base:1\n"),
            ),
            (
                "main:.npmrc",
                format!("registry=https://{HOST}/npm/npmjs/\n"),
            ),
        ];
        let bare: Vec<(&str, String)> =
            vec![("main:.gitlab-ci.yml", "script: echo hi\n".to_string())];
        let srv = fake_gitlab(vec![
            fake_project(1, "good/a", &["main"], &wired),
            fake_project(2, "good/b", &["main"], &wired),
            fake_project(3, "bad/a", &["main"], &bare),
        ])
        .await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let groups = scanner.group_coverage();
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(
            (groups[0].group.as_str(), groups[0].percent),
            ("bad", 0),
            "want the worst one first"
        );
        assert_eq!((groups[1].group.as_str(), groups[1].percent), ("good", 100));
    }

    #[tokio::test]
    async fn not_applied_orders_missing_before_partial() {
        let srv = fake_gitlab(vec![
            fake_project(
                1,
                "z/partial",
                &["main"],
                &[
                    (
                        "main:.gitlab-ci.yml",
                        format!("image: {HOST}/docker/base:1\n"),
                    ),
                    (
                        "main:.npmrc",
                        "registry=https://registry.npmjs.org/\n".to_string(),
                    ),
                ],
            ),
            fake_project(
                2,
                "a/missing",
                &["main"],
                &[("main:.gitlab-ci.yml", "script: echo hi\n".to_string())],
            ),
        ])
        .await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let got = scanner.not_applied();
        assert_eq!(got.len(), 2, "{got:?}");
        // A project with no wiring is further from done than one with half of it,
        // so it heads the list the report is built from.
        assert_eq!(got[0].path, "a/missing");
        assert_eq!(got[1].path, "z/partial");
    }

    #[tokio::test]
    async fn pipeline_returns_only_ci_files() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main"],
            &[
                (
                    "main:.gitlab-ci.yml",
                    format!("image: {HOST}/docker/base:1\n"),
                ),
                (
                    "main:.npmrc",
                    format!("registry=https://{HOST}/npm/npmjs/\n"),
                ),
                (
                    "main:src/secret.go",
                    "package main // proprietary".to_string(),
                ),
            ],
        )])
        .await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let pipeline = scanner
            .pipeline("team/app", "main")
            .await
            .expect("Pipeline");
        assert_eq!(pipeline.files.len(), 1, "{:?}", pipeline.files);
        assert_eq!(pipeline.files[0].path, ".gitlab-ci.yml");
        assert!(
            pipeline.files[0].matches_forklift,
            "the CI file references the host but was not flagged"
        );
        // The allowlist is the security boundary: source and registry config are
        // both outside it, however the request is shaped.
        for f in &pipeline.files {
            assert!(
                !f.path.contains("secret.go") && !f.path.contains(".npmrc"),
                "the pipeline viewer returned {:?}, which is not a CI definition",
                f.path
            );
        }
    }

    #[tokio::test]
    async fn last_commit_uses_the_verdict_branch() {
        let srv = fake_gitlab(vec![fake_project(
            1,
            "team/app",
            &["main", "feature/forklift"],
            &[
                ("main:.gitlab-ci.yml", "script: echo hi\n".to_string()),
                (
                    "feature/forklift:.gitlab-ci.yml",
                    format!("image: {HOST}/docker/base:1\n"),
                ),
                (
                    "feature/forklift:.npmrc",
                    format!("registry=https://{HOST}/npm/npmjs/\n"),
                ),
            ],
        )])
        .await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        scanner.scan("tester").await.expect("Scan");
        let commit = scanner
            .last_commit("team/app")
            .await
            .expect("LastCommit")
            .expect("a commit");
        assert_eq!(
            commit.ref_, "feature/forklift",
            "want the branch the verdict came from"
        );
    }

    #[tokio::test]
    async fn project_detail_falls_back_to_gitlab() {
        // A project created since the last scan still resolves, so a valid path
        // never dead-ends and can be toggled in or out of scope.
        let srv = fake_gitlab(vec![fake_project(7, "team/new", &["main"], &[])]).await;
        let scanner = new_test_scanner(new_mem_store(with_host(HOST)), &srv, None);
        let detail = scanner
            .project_detail("team/new")
            .await
            .expect("ProjectDetail");
        assert!(
            !detail.scanned,
            "scanned = true for a project the last scan never covered"
        );
        assert_eq!(
            detail.project.path, "team/new",
            "want the project looked up from GitLab"
        );
    }
}
