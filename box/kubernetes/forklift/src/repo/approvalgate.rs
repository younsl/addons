//! The fixed final boundary: human package approval, plus the always-enforced
//! per-version deny list.

use std::sync::Arc;
use std::time::Duration;

use http::request::Parts;
use http::{Method, StatusCode};

use crate::repoconfig::{self, Config, MODE_AUDIT, MODE_ENFORCE};
use crate::server::http_error;
use crate::{audit, auth, meta, vuln};

use super::router::HandlerFutureOpt;
use super::upload::approval_bypassed;
use super::vulnscan::osv_ecosystem;
use super::{Manager, Resolved};

/// Auto-approve-clean decision provenance, recorded on the approval row and its
/// audit event so a clean-scan admission is distinguishable from a human one.
pub(crate) const AUTO_APPROVE_ACTOR: &str = "system:auto-approve-clean";
const AUTO_APPROVE_NOTE: &str = "auto-approved: clean vulnerability scan (max severity none)";

/// Suppresses repeated pending-approval upserts for the same (repo, package) so
/// unapproved hot paths do not hammer the single-writer SQLite. One mark per
/// instance; a duplicate upsert after failover is idempotent.
pub(crate) const PENDING_MARK_TTL: Duration = Duration::from_secs(60);

impl Manager {
    /// Enforces the package approval policy (quarantine) for proxy and hosted
    /// repositories. It is the final serving boundary, after automated policies
    /// and age evaluation. A rejected package may already be cached, but its
    /// bytes are never returned to the client.
    ///
    /// `Some(response)` means the request was blocked. Blocks use 403, not 404:
    /// the group fan-out treats 404 as a member miss and would silently serve
    /// the package from the next member, and the GOPROXY protocol falls back to
    /// the next proxy on 404 — both would bypass the gate.
    ///
    /// `version` is the exact version derived from the request path (`""` for
    /// metadata requests). Version deny and automated policies are evaluated
    /// independently by [`Manager::policy_gates`] before this package-level
    /// workflow.
    pub(crate) fn approval_gate(
        m: Arc<Manager>,
        parts: Arc<Parts>,
        res: Arc<Resolved>,
        pkg: String,
        version: String,
    ) -> HandlerFutureOpt {
        Box::pin(async move {
            if res.repo.r#type != meta::TYPE_PROXY && res.repo.r#type != meta::TYPE_HOSTED {
                return None;
            }
            if parts.method != Method::GET && parts.method != Method::HEAD {
                return None;
            }
            if !res.cfg.approval.enabled {
                return None;
            }
            // Never block when the package name cannot be derived from the path
            // (mirrors the age policy's missing-published_at behavior).
            if pkg.is_empty() {
                return None;
            }
            if approval_bypassed(&res.cfg, &pkg) {
                return None;
            }

            let mode = res.cfg.approval.effective_mode().to_string();
            let status = m.store.get_approval_status(&res.repo.name, &pkg).await;
            match &status {
                Err(meta::Error::NotFound) => {}
                Err(err) => {
                    tracing::error!(
                        repo = %res.repo.name, package = %pkg, err = %err,
                        "approval status lookup failed"
                    );
                    // Fail closed in enforce mode, open in audit mode.
                    if mode == MODE_ENFORCE {
                        return Some(http_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "approval check failed",
                        ));
                    }
                    return None;
                }
                Ok(s) if s == meta::APPROVAL_APPROVED => return None,
                Ok(_) => {}
            }

            // Auto-approve on a clean vulnerability verdict: when enabled and
            // the requested coordinate has a stored scan with no advisories (max
            // severity "none"), admit the whole package automatically rather
            // than queuing it for review. Applies in both enforce and audit
            // modes. The version is whatever the request carried — a specific
            // version for an artifact fetch, or "" for a metadata request (npm
            // packument, PyPI simple index), which matches the package-level
            // scan the block path below enqueues. Until a verdict exists the
            // request falls through and is blocked/would-blocked (queuing the
            // scan), so a later request auto-approves once the async scan lands
            // clean. Automated vulnerability evaluation runs before this final
            // gate on every request.
            if res.cfg.approval.auto_approve_clean
                && m.auto_approve_if_clean(&parts, &res, &pkg, &version).await
            {
                return None;
            }

            // Record demand, suppressed per (repo, package) to bound write/log
            // volume.
            let mark = format!("{}\u{0}{pkg}", res.repo.name);
            if !m.req_marks.has(&mark) {
                m.req_marks.set(&mark, PENDING_MARK_TTL);
                let username = auth::from_request_parts(&parts)
                    .map(|p| p.username.clone())
                    .unwrap_or_default();
                // Scan the requested coordinate so the would-block inventory
                // carries a vulnerability signal for the reviewer. A known
                // version scans precisely; an unknown version (e.g. a blocked
                // npm packument) falls back to a package-level scan. No-op when
                // scanning is disabled.
                m.enqueue_scan(osv_ecosystem(&res.repo.format), &pkg, &version);

                if mode == MODE_AUDIT {
                    // Audit mode serves the package, so there is no decision to
                    // make and nothing to unblock: a "pending" approval row
                    // would misrepresent the queue, whose rows mean "actively
                    // blocked, awaiting a decision". The approval.request audit
                    // event is the inventory of what enforce mode would block;
                    // no outbound alarm fires, since nothing is blocked.
                    m.record_approval_request(&parts, &res.repo.name, &pkg);
                } else {
                    match m
                        .store
                        .upsert_pending_approval(&res.repo.name, &pkg, &username, &version)
                        .await
                    {
                        Err(err) => tracing::error!(
                            repo = %res.repo.name, package = %pkg, err = %err,
                            "pending approval upsert failed"
                        ),
                        Ok(true) => {
                            m.record_approval_request(&parts, &res.repo.name, &pkg);
                            // Fire the outbound alarm only for a genuinely new
                            // request, matching the audit event and avoiding
                            // duplicate alerts on retries.
                            let hook = m.on_approval.read().clone();
                            if let Some(on_approval) = hook {
                                on_approval(
                                    res.repo.name.clone(),
                                    res.repo.id,
                                    res.repo.format.clone(),
                                    pkg.clone(),
                                    version.clone(),
                                    username.clone(),
                                    res.cfg.notify.receivers.clone(),
                                );
                            }
                        }
                        Ok(false) => {}
                    }
                }
            }

            m.approval_blocked
                .with_label_values(&[&res.repo.name, &mode])
                .inc();
            if mode == MODE_AUDIT {
                tracing::warn!(
                    repo = %res.repo.name, package = %pkg, status = %status_or_none(&status),
                    "approval audit: would block"
                );
                return None;
            }
            tracing::warn!(
                repo = %res.repo.name, package = %pkg, status = %status_or_none(&status),
                "package blocked pending approval"
            );
            Some(http_error(
                StatusCode::FORBIDDEN,
                &format!("package pending approval: {pkg}"),
            ))
        })
    }

    /// Admits a package when the requested coordinate has a stored vulnerability
    /// scan with no advisories (max severity "none"). It persists an approved
    /// decision, records the `approval.approve` audit event and bumps the
    /// auto-approve metric, and reports whether the package was admitted. It
    /// never blocks: a disabled scanner, an unsupported ecosystem, a
    /// missing/vulnerable verdict, or a persist error all return false so the
    /// caller falls through to normal gating (which queues the scan for a later
    /// retry).
    async fn auto_approve_if_clean(
        &self,
        parts: &Parts,
        res: &Resolved,
        pkg: &str,
        version: &str,
    ) -> bool {
        if self.scanner.read().is_none() {
            return false;
        }
        let eco = osv_ecosystem(&res.repo.format);
        if eco.is_empty() {
            return false;
        }
        let scan = match self.store.get_vuln_scan(eco, pkg, version).await {
            Ok(scan) => scan,
            Err(_) => return false,
        };
        if vuln::parse_severity(&scan.max_severity) != vuln::Severity::None {
            return false;
        }
        if let Err(err) = self
            .store
            .upsert_approval_decision(
                &res.repo.name,
                pkg,
                meta::APPROVAL_APPROVED,
                AUTO_APPROVE_ACTOR,
                AUTO_APPROVE_NOTE,
            )
            .await
        {
            tracing::error!(
                repo = %res.repo.name, package = %pkg, err = %err, "auto-approve clean failed"
            );
            return false;
        }
        self.approval_auto_approved
            .with_label_values(&[&res.repo.name])
            .inc();
        tracing::info!(
            repo = %res.repo.name, package = %pkg, version = %version,
            "package auto-approved by clean vulnerability scan"
        );
        if let Some(rec) = &self.rec {
            rec.record(audit::Event {
                repo: res.repo.name.clone(),
                action: meta::EVENT_APPROVAL_APPROVE.to_string(),
                path: pkg.to_string(),
                username: AUTO_APPROVE_ACTOR.to_string(),
                method: parts.method.to_string(),
                status: StatusCode::OK.as_u16() as i64,
                client_ip: audit::client_ip_parts(parts),
                user_agent: super::header_str(&parts.headers, "User-Agent").to_string(),
                ..Default::default()
            });
        }
        true
    }

    /// Applies the package-approval policy to a publication produced by the
    /// managed upload path, mirroring `queue_hosted_approval` but without an
    /// HTTP request (the caller is the async publish pipeline). It is the
    /// `Uploader::set_publish_hook` target wired in [`Manager::set_uploader`].
    /// Scanning and license resolution are handled separately via
    /// `Engine::on_store`; this only governs the approval quarantine so uploaded
    /// packages enter the review queue immediately instead of on their first
    /// download.
    pub(crate) async fn quarantine_uploaded_publication(
        &self,
        repository: &meta::Repository,
        pkg: &str,
        version: &str,
        username: &str,
    ) {
        if pkg.is_empty() {
            return;
        }
        let cfg: Config = match repoconfig::parse(&repository.config_json) {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::error!(
                    repo = %repository.name, err = %err,
                    "approval config parse failed for upload"
                );
                return;
            }
        };
        if !cfg.approval.enabled || approval_bypassed(&cfg, pkg) {
            return;
        }
        if cfg.approval.effective_mode() == MODE_AUDIT {
            // Audit mode never blocks and never queues a decision row; the audit
            // event alone records what enforce mode would have quarantined.
            self.record_upload_approval_request(&repository.name, pkg, username);
            return;
        }
        let created = match self
            .store
            .upsert_pending_approval(&repository.name, pkg, username, version)
            .await
        {
            Ok(created) => created,
            Err(err) => {
                tracing::error!(
                    repo = %repository.name, package = %pkg, err = %err,
                    "pending approval upsert failed for upload"
                );
                return;
            }
        };
        if created {
            self.record_upload_approval_request(&repository.name, pkg, username);
            let hook = self.on_approval.read().clone();
            if let Some(on_approval) = hook {
                on_approval(
                    repository.name.clone(),
                    repository.id,
                    repository.format.clone(),
                    pkg.to_string(),
                    version.to_string(),
                    username.to_string(),
                    cfg.notify.receivers.clone(),
                );
            }
        }
    }

    /// Logs the `approval.request` audit event for an upload-time quarantine.
    /// Unlike [`Manager::record_approval_request`] it has no HTTP request, so
    /// client-IP/method/user-agent are omitted.
    fn record_upload_approval_request(&self, repo_name: &str, pkg: &str, username: &str) {
        let Some(rec) = &self.rec else {
            return;
        };
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_APPROVAL_REQUEST.to_string(),
            path: pkg.to_string(),
            username: username.to_string(),
            ..Default::default()
        });
    }

    /// Logs the `approval.request` audit event for a package that needs
    /// approval: blocked pending a decision in enforce mode, would-block (but
    /// served) in audit mode. Status is 403 in both, so the event reads as a
    /// uniform "would-block" signal regardless of the repository's current mode.
    pub(crate) fn record_approval_request(&self, parts: &Parts, repo_name: &str, pkg: &str) {
        let Some(rec) = &self.rec else {
            return;
        };
        let username = auth::from_request_parts(parts)
            .map(|p| p.username.clone())
            .unwrap_or_default();
        rec.record(audit::Event {
            repo: repo_name.to_string(),
            action: meta::EVENT_APPROVAL_REQUEST.to_string(),
            path: pkg.to_string(),
            username,
            method: parts.method.to_string(),
            status: StatusCode::FORBIDDEN.as_u16() as i64,
            client_ip: audit::client_ip_parts(parts),
            user_agent: super::header_str(&parts.headers, "User-Agent").to_string(),
            ..Default::default()
        });
    }

    /// Blocks requests for explicitly denied (package, version) pairs. It runs
    /// before any cache lookup or upstream fetch, so denying a version
    /// immediately cuts off already-cached copies too. Metadata requests
    /// (`version == ""`) pass through: a denied version stays listed in
    /// packuments and indexes, but its artifact fetch fails loudly with 403 —
    /// for a poisoned release a loud failure beats the resolver silently picking
    /// another version.
    pub(crate) fn version_deny_gate(
        m: Arc<Manager>,
        parts: Arc<Parts>,
        res: Arc<Resolved>,
        pkg: String,
        version: String,
    ) -> HandlerFutureOpt {
        Box::pin(async move {
            if res.repo.r#type != meta::TYPE_PROXY && res.repo.r#type != meta::TYPE_HOSTED {
                return None;
            }
            if parts.method != Method::GET && parts.method != Method::HEAD {
                return None;
            }
            if pkg.is_empty() || version.is_empty() {
                return None;
            }
            let denied = match m
                .store
                .is_version_denied(&res.repo.name, &pkg, &version)
                .await
            {
                Ok(denied) => denied,
                Err(err) => {
                    // Fail closed: a deny is an always-enforce control, never
                    // silently skipped (unlike audit-mode approval lookups).
                    tracing::error!(
                        repo = %res.repo.name, package = %pkg, version = %version, err = %err,
                        "version deny lookup failed"
                    );
                    return Some(http_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "deny check failed",
                    ));
                }
            };
            if !denied {
                return None;
            }

            // Audit each blocked (repo, package, version) at most once per mark
            // TTL, mirroring the pending-approval suppression.
            let mark = format!("deny\u{0}{}\u{0}{pkg}\u{0}{version}", res.repo.name);
            if !m.req_marks.has(&mark) {
                m.req_marks.set(&mark, PENDING_MARK_TTL);
                if let Some(rec) = &m.rec {
                    let username = auth::from_request_parts(&parts)
                        .map(|p| p.username.clone())
                        .unwrap_or_default();
                    rec.record(audit::Event {
                        repo: res.repo.name.clone(),
                        action: meta::EVENT_DENY_BLOCK.to_string(),
                        path: format!("{pkg}@{version}"),
                        username,
                        method: parts.method.to_string(),
                        status: StatusCode::FORBIDDEN.as_u16() as i64,
                        client_ip: audit::client_ip_parts(&parts),
                        user_agent: super::header_str(&parts.headers, "User-Agent").to_string(),
                        ..Default::default()
                    });
                }
            }

            m.deny_blocked.with_label_values(&[&res.repo.name]).inc();
            tracing::warn!(
                repo = %res.repo.name, package = %pkg, version = %version,
                "version blocked by deny list"
            );
            Some(http_error(
                StatusCode::FORBIDDEN,
                &format!("version denied: {pkg}@{version}"),
            ))
        })
    }
}

/// Renders an approval status for logging, with "none" for packages that have no
/// approval row yet.
fn status_or_none(status: &Result<String, meta::Error>) -> String {
    match status {
        Ok(s) => s.clone(),
        Err(_) => "none".to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

    use axum::Router;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
    use http::{Method, StatusCode};

    use crate::meta;
    use crate::repoconfig::{self, ApprovalConfig, Config, MODE_AUDIT};

    use crate::repo::approvalgate::AUTO_APPROVE_ACTOR;
    use crate::repo::cargo::cargo_package;
    use crate::repo::gomod::go_package;
    use crate::repo::maven::maven_package;
    use crate::repo::npm::{npm_package, npm_version};
    use crate::repo::pypi::pypi_package_from_filename;
    use crate::repo::vulngate::tests::FakeScanner;
    use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, spawn_upstream};

    /// One extractor table row: the function, its input, and the expected result.
    type ExtractorCase = (fn(&str) -> String, &'static str, &'static str);

    #[test]
    fn package_extractors() {
        let cases: &[ExtractorCase] = &[
            (npm_package, "lodash", "lodash"),
            (npm_package, "lodash/-/lodash-4.17.21.tgz", "lodash"),
            (npm_package, "@scope/name", "@scope/name"),
            (npm_package, "@scope/name/-/name-1.0.0.tgz", "@scope/name"),
            // npm publish: lowercase %2f
            (npm_package, "@scope%2fname", "@scope/name"),
            // pnpm/others: uppercase %2F
            (npm_package, "@scope%2Fname", "@scope/name"),
            // encoded @ + slash (e.g. @openai%2fcodex form)
            (npm_package, "%40scope%2fname", "@scope/name"),
            // case-folded to canonical lowercase
            (npm_package, "@Scope%2FName", "@scope/name"),
            (npm_package, "@scope%2fname/-/name-1.0.0.tgz", "@scope/name"),
            // version resolves through encoded scope
            (npm_version, "@scope%2fname/-/name-1.0.0.tgz", "1.0.0"),
            (
                pypi_package_from_filename,
                "requests-2.31.0-py3-none-any.whl",
                "requests",
            ),
            (
                pypi_package_from_filename,
                "typing_extensions-4.8.0.tar.gz",
                "typing-extensions",
            ),
            (pypi_package_from_filename, "Foo.Bar-1.0.zip", "foo-bar"),
            (
                pypi_package_from_filename,
                "requests-2.31.0-py3-none-any.whl.metadata",
                "requests",
            ),
            (pypi_package_from_filename, "noversion", ""),
            (cargo_package, "api/v1/crates/serde/1.0.0/download", "serde"),
            (cargo_package, "se/rd/serde", "serde"),
            (cargo_package, "3/a/aes", "aes"),
            (cargo_package, "1/x", "x"),
            (cargo_package, "config.json", ""),
            (go_package, "example.com/foo/@v/list", "example.com/foo"),
            (
                go_package,
                "example.com/foo/@v/v1.0.0.zip",
                "example.com/foo",
            ),
            (go_package, "example.com/foo/@latest", "example.com/foo"),
            (go_package, "example.com/foo", ""),
            (
                maven_package,
                "com/google/guava/guava/31.0/guava-31.0.jar",
                "com.google.guava:guava",
            ),
            (
                maven_package,
                "com/google/guava/guava/maven-metadata.xml",
                "com.google.guava:guava",
            ),
            (
                maven_package,
                "com/google/guava/guava/1.0-SNAPSHOT/maven-metadata.xml",
                "com.google.guava:guava",
            ),
            (
                maven_package,
                "junit/junit/4.13/junit-4.13.jar",
                "junit:junit",
            ),
            (maven_package, "junit/maven-metadata.xml", ""),
            (maven_package, "short", ""),
            (npm_version, "lodash", ""),
            (npm_version, "lodash/-/lodash-4.17.21.tgz", "4.17.21"),
            (
                npm_version,
                "lodash/-/lodash-1.0.0-beta.1.tgz",
                "1.0.0-beta.1",
            ),
            (npm_version, "@scope/name/-/name-1.0.0.tgz", "1.0.0"),
            (npm_version, "pkg/-/other-1.0.0.tgz", ""),
            (npm_version, "pkg/-/pkg-1.0.0.bad", ""),
        ];
        for (f, input, want) in cases {
            assert_eq!(f(input).as_str(), *want, "extract({input:?})");
        }
    }

    async fn tarball_upstream() -> String {
        spawn_upstream(Router::new().fallback(|| async { "tarball-bytes" })).await
    }

    /// A proxy config with the approval policy enabled.
    pub(crate) fn approval_cfg(mode: &str, auto_approve: &[&str]) -> Config {
        let mut cfg = repoconfig::default();
        cfg.approval = ApprovalConfig {
            enabled: true,
            mode: mode.to_string(),
            auto_approve: auto_approve.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        cfg
    }

    #[tokio::test]
    async fn version_deny_gate() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        // Approval workflow OFF: the deny list must still enforce.
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        // Cache the tarball first, then deny it: cached copies must be revoked.
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "pre-deny fetch");
        tm.store
            .upsert_version_deny("npmjs", "lodash", "4.17.99", "IOC", "sec")
            .await
            .expect("upsert deny");

        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "denied version");
        assert!(
            resp.text().contains("version denied"),
            "body = {:?}",
            resp.text()
        );
        // Other versions of the same package keep flowing.
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.21.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "other version");
        // Metadata requests (version == "") are not blocked.
        let resp = call(&h, Method::GET, "/npm/npmjs/lodash", "").await;
        assert_ne!(
            resp.status,
            StatusCode::FORBIDDEN,
            "packument must not be blocked by a version deny"
        );

        // Un-deny: traffic resumes.
        let rows = tm
            .store
            .list_version_denies("npmjs", 10, 0)
            .await
            .expect("list denies");
        assert_eq!(rows.len(), 1, "denies");
        tm.store
            .delete_version_deny(rows[0].id)
            .await
            .expect("delete deny");
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "after un-deny");
    }

    #[tokio::test]
    async fn version_deny_overrides_approval() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        // Audit mode never blocks on approval status; the deny must still enforce.
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            approval_cfg(MODE_AUDIT, &[]),
        )
        .await;
        let h = mux(&tm.manager);

        tm.store
            .upsert_approval_decision("npmjs", "lodash", meta::APPROVAL_APPROVED, "admin", "")
            .await
            .expect("approve");
        tm.store
            .upsert_version_deny("npmjs", "lodash", "4.17.99", "poisoned release", "sec")
            .await
            .expect("deny");

        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "deny must override approval (audit mode)"
        );
    }

    /// Serves a fixed packument, counting upstream hits.
    async fn packument_upstream(body: &'static str) -> (String, Arc<AtomicI32>) {
        let hits = Arc::new(AtomicI32::new(0));
        let counter = Arc::clone(&hits);
        let url = spawn_upstream(Router::new().fallback(move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                body
            }
        }))
        .await;
        (url, hits)
    }

    #[tokio::test]
    async fn approval_gate_enforce() {
        let (upstream, upstream_hits) =
            packument_upstream(r#"{"name":"left-pad","versions":{},"time":{}}"#).await;
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            approval_cfg("", &[]),
        )
        .await;
        let h = mux(&tm.manager);

        // Unapproved: upstream data may be cached, but final approval still blocks
        // both metadata and artifact responses.
        for p in [
            "/npm/npmjs/left-pad",
            "/npm/npmjs/left-pad/-/left-pad-1.3.0.tgz",
        ] {
            let resp = call(&h, Method::GET, p, "").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "{p}");
            assert!(
                resp.text().contains("pending approval"),
                "{p}: body = {:?}",
                resp.text()
            );
        }
        assert!(
            upstream_hits.load(Ordering::SeqCst) > 0,
            "final approval should run after upstream fetch and age evaluation"
        );

        // Repeated requests dedup into one pending row (write-suppressed).
        let rows = tm
            .store
            .list_approvals("npmjs", meta::APPROVAL_PENDING, 10, 0)
            .await
            .expect("list approvals");
        assert_eq!(rows.len(), 1, "pending rows");
        assert_eq!(rows[0].package, "left-pad");

        // Approve: traffic flows.
        tm.store
            .decide_approval(rows[0].id, meta::APPROVAL_APPROVED, "admin", "ok")
            .await
            .expect("approve");
        let resp = call(&h, Method::GET, "/npm/npmjs/left-pad", "").await;
        assert_eq!(resp.status, StatusCode::OK, "approved packument");
        assert!(
            upstream_hits.load(Ordering::SeqCst) > 0,
            "approved package should reach upstream"
        );

        // Reject after the packument is cached: served content is revoked
        // immediately.
        tm.store
            .decide_approval(rows[0].id, meta::APPROVAL_REJECTED, "admin", "incident")
            .await
            .expect("reject");
        let resp = call(&h, Method::GET, "/npm/npmjs/left-pad", "").await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "rejected packument");
    }

    #[tokio::test]
    async fn approval_gate_auto_approve_and_hosted() {
        let (upstream, _) = packument_upstream(r#"{"name":"x","versions":{},"time":{}}"#).await;
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            approval_cfg("", &["@company/*"]),
        )
        .await;
        // Hosted repos are never gated even with approval enabled in config.
        mk_format_repo(
            &tm.store,
            "npm-hosted",
            meta::FORMAT_NPM,
            meta::TYPE_HOSTED,
            "",
            approval_cfg("", &[]),
        )
        .await;
        let h = mux(&tm.manager);

        let resp = call(&h, Method::GET, "/npm/npmjs/@company/lib", "").await;
        assert_eq!(resp.status, StatusCode::OK, "auto-approved");
        assert_eq!(
            tm.store.count_approvals("npmjs", "").await.unwrap_or(0),
            0,
            "auto-approve must not create approval rows"
        );

        let resp = call(&h, Method::GET, "/npm/npm-hosted/anything", "").await;
        assert_ne!(
            resp.status,
            StatusCode::FORBIDDEN,
            "hosted repo must not be gated"
        );
    }

    #[tokio::test]
    async fn approval_gate_audit_mode() {
        let (upstream, _) =
            packument_upstream(r#"{"name":"left-pad","versions":{},"time":{}}"#).await;
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            approval_cfg(MODE_AUDIT, &[]),
        )
        .await;
        let h = mux(&tm.manager);

        let resp = call(&h, Method::GET, "/npm/npmjs/left-pad", "").await;
        assert_eq!(resp.status, StatusCode::OK, "audit mode must serve");
        // Audit mode serves the package, so it must not populate the approval
        // queue: pending rows mean "actively blocked, awaiting a decision". Demand
        // is recorded in the audit log (approval.request) as a would-block inventory
        // instead — see `record_approval_request`.
        assert_eq!(
            tm.store.count_approvals("npmjs", "").await.unwrap_or(0),
            0,
            "audit mode must not create approval rows"
        );
    }

    /// Covers the clean-scan auto-approval: with a scanner configured and
    /// `auto_approve_clean` on, a package whose stored scan has no advisories (max
    /// severity "none") is admitted automatically, while a vulnerable or
    /// not-yet-scanned package still blocks pending review.
    #[tokio::test]
    async fn approval_gate_auto_approve_clean() {
        let (upstream, upstream_hits) =
            packument_upstream(r#"{"name":"x","versions":{},"time":{}}"#).await;
        let tm = new_test_manager().await;
        // Activates auto-approve-clean; the async query is unused.
        tm.manager.set_vuln_scanner(Some(Arc::new(FakeScanner)));
        let mut cfg = repoconfig::default();
        // Mode "" = enforce.
        cfg.approval = ApprovalConfig {
            enabled: true,
            auto_approve_clean: true,
            ..Default::default()
        };
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            cfg,
        )
        .await;
        let h = mux(&tm.manager);

        // Clean package-level scan (an npm packument carries no version) ->
        // auto-approved.
        tm.store
            .upsert_vuln_scan(
                "npm",
                "clean-pkg",
                "",
                "none",
                &[],
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert scan");
        let resp = call(&h, Method::GET, "/npm/npmjs/clean-pkg", "").await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "clean package must auto-approve and serve"
        );
        assert!(
            upstream_hits.load(Ordering::SeqCst) > 0,
            "auto-approved package should reach upstream"
        );
        let status = tm
            .store
            .get_approval_status("npmjs", "clean-pkg")
            .await
            .expect("approval status");
        assert_eq!(status, meta::APPROVAL_APPROVED);
        let rows = tm
            .store
            .list_approvals("npmjs", meta::APPROVAL_APPROVED, 10, 0)
            .await
            .unwrap_or_default();
        assert_eq!(rows.len(), 1, "auto-approval row");
        assert_eq!(rows[0].decided_by, AUTO_APPROVE_ACTOR);

        // Vulnerable package -> still blocked, not auto-approved.
        tm.store
            .upsert_vuln_scan(
                "npm",
                "vuln-pkg",
                "",
                "critical",
                &["CVE-2026-9".to_string()],
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert scan");
        let resp = call(&h, Method::GET, "/npm/npmjs/vuln-pkg", "").await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "vulnerable package must block"
        );
        assert_ne!(
            tm.store
                .get_approval_status("npmjs", "vuln-pkg")
                .await
                .unwrap_or_default(),
            meta::APPROVAL_APPROVED,
            "vulnerable package must not be auto-approved"
        );

        // Not-yet-scanned package -> blocked pending review (a later request
        // auto-approves once its async scan lands clean).
        let resp = call(&h, Method::GET, "/npm/npmjs/unscanned", "").await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "unscanned package must block"
        );
        assert_ne!(
            tm.store
                .get_approval_status("npmjs", "unscanned")
                .await
                .unwrap_or_default(),
            meta::APPROVAL_APPROVED,
            "unscanned package must not be auto-approved"
        );
    }

    #[tokio::test]
    async fn approval_gate_pypi_and_group() {
        let upstream = spawn_upstream(Router::new().fallback(|| async {
            r#"{"meta":{"api-version":"1.1"},"name":"requests","files":[]}"#
        }))
        .await;
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "pypi-gated",
            meta::FORMAT_PYPI,
            meta::TYPE_PROXY,
            &upstream,
            approval_cfg("", &[]),
        )
        .await;
        mk_format_repo(
            &tm.store,
            "pypi-open",
            meta::FORMAT_PYPI,
            meta::TYPE_PROXY,
            &upstream,
            repoconfig::default(),
        )
        .await;
        let mut group_cfg = repoconfig::default();
        group_cfg.group.members = vec!["pypi-gated".to_string(), "pypi-open".to_string()];
        mk_format_repo(
            &tm.store,
            "pypi-all",
            meta::FORMAT_PYPI,
            meta::TYPE_GROUP,
            "",
            group_cfg,
        )
        .await;
        let h = mux(&tm.manager);

        // Simple index and file paths are both gated.
        let file_ref = BASE64URL.encode(format!("{upstream}/f").as_bytes());
        for p in [
            "/pypi/pypi-gated/simple/requests/".to_string(),
            format!("/pypi/pypi-gated/packages/{file_ref}/requests-2.31.0-py3-none-any.whl"),
        ] {
            let resp = call(&h, Method::GET, &p, "").await;
            assert_eq!(resp.status, StatusCode::FORBIDDEN, "{p}");
        }

        // A gated member's 403 is authoritative for the group: no fall-through to
        // the open member (that would bypass the gate).
        let resp = call(&h, Method::GET, "/pypi/pypi-all/simple/requests/", "").await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "group: no member fall-through"
        );
    }
}
