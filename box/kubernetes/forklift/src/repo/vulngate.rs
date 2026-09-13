//! Gates a coordinate on its stored vulnerability scan.

use std::sync::Arc;

use http::request::Parts;
use http::{Method, StatusCode};

use crate::repoconfig::{VULN_ACTION_AUDIT, VULN_ACTION_BLOCK, VULN_ACTION_WARN};
use crate::server::http_error;
use crate::{audit, auth, meta, vuln};

use super::router::HandlerFutureOpt;
use super::vulnscan::osv_ecosystem;
use super::{Manager, Resolved};

impl Manager {
    /// Enforces the per-repository vulnerability policy for proxy and hosted
    /// reads. It consults stored scan results only (never blocking on a live
    /// lookup): a not-yet-scanned coordinate is queued for async scanning and,
    /// unless `block_unscanned` is set, served meanwhile. A scanned coordinate
    /// whose remaining (non-ignored) advisories meet the threshold is blocked,
    /// warned, or audited.
    ///
    /// `Some(response)` means the request was blocked.
    pub(crate) fn vuln_gate(
        m: Arc<Manager>,
        parts: Arc<Parts>,
        res: Arc<Resolved>,
        pkg: String,
        version: String,
    ) -> HandlerFutureOpt {
        Box::pin(async move {
            if m.scanner.read().is_none()
                || (res.repo.r#type != meta::TYPE_PROXY && res.repo.r#type != meta::TYPE_HOSTED)
            {
                return None;
            }
            if parts.method != Method::GET && parts.method != Method::HEAD {
                return None;
            }
            let cfg = &res.cfg.vuln;
            if !cfg.enabled || pkg.is_empty() || version.is_empty() {
                return None;
            }
            let eco = osv_ecosystem(&res.repo.format);
            if eco.is_empty() {
                return None;
            }

            let scan = match m.store.get_vuln_scan(eco, &pkg, &version).await {
                Ok(scan) => scan,
                Err(meta::Error::NotFound) => {
                    m.enqueue_scan(eco, &pkg, &version);
                    // Unknown coordinate: fail open unless the policy opts into
                    // blocking pending scans under an enforcing posture.
                    if cfg.effective_action() == VULN_ACTION_BLOCK && cfg.block_unscanned {
                        return Some(http_error(
                            StatusCode::FORBIDDEN,
                            &format!("package pending vulnerability scan: {pkg}"),
                        ));
                    }
                    return None;
                }
                Err(err) => {
                    // Best-effort: a lookup error must not break serving.
                    tracing::error!(
                        repo = %res.repo.name, package = %pkg, version = %version, err = %err,
                        "vuln scan lookup failed"
                    );
                    return None;
                }
            };

            // All advisories accepted/false-positive: treat as clean.
            if scan.vuln_ids.is_empty() || all_ignored(&scan.vuln_ids, &cfg.ignore) {
                return None;
            }
            if vuln::parse_severity(&scan.max_severity)
                < vuln::parse_severity(cfg.effective_threshold())
            {
                return None;
            }

            let action = cfg.effective_action();
            m.vuln_blocked
                .with_label_values(&[&res.repo.name, action])
                .inc();
            if action == VULN_ACTION_AUDIT || action == VULN_ACTION_WARN {
                tracing::warn!(
                    repo = %res.repo.name, package = %pkg, version = %version,
                    severity = %scan.max_severity, ids = %scan.vuln_ids.join(","), action,
                    "vuln policy: would block"
                );
                return None;
            }

            if let Some(rec) = &m.rec {
                let username = auth::from_request_parts(&parts)
                    .map(|p| p.username.clone())
                    .unwrap_or_default();
                rec.record(audit::Event {
                    repo: res.repo.name.clone(),
                    action: meta::EVENT_VULN_BLOCK.to_string(),
                    path: format!("{pkg}@{version}"),
                    username,
                    method: parts.method.to_string(),
                    status: StatusCode::FORBIDDEN.as_u16() as i64,
                    client_ip: audit::client_ip_parts(&parts),
                    user_agent: super::header_str(&parts.headers, "User-Agent").to_string(),
                    ..Default::default()
                });
            }
            tracing::warn!(
                repo = %res.repo.name, package = %pkg, version = %version,
                severity = %scan.max_severity, ids = %scan.vuln_ids.join(","),
                "package blocked by vulnerability policy"
            );
            Some(http_error(
                StatusCode::FORBIDDEN,
                &format!(
                    "blocked: known vulnerabilities ({}) in {pkg}@{version}",
                    scan.max_severity
                ),
            ))
        })
    }
}

/// Reports whether every advisory id is in the ignore list.
fn all_ignored(ids: &[String], ignore: &[String]) -> bool {
    if ignore.is_empty() {
        return false;
    }
    let set: std::collections::HashSet<&str> = ignore.iter().map(String::as_str).collect();
    ids.iter().all(|id| set.contains(id.as_str()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::Router;
    use http::{Method, StatusCode};
    use parking_lot::Mutex;

    use crate::meta::{self, Artifact};
    use crate::repoconfig::{
        self, Config, SEVERITY_HIGH, VULN_ACTION_AUDIT, VULN_ACTION_BLOCK, VulnPolicyConfig,
    };
    use crate::vuln::{self, Finding};

    use crate::repo::Manager;
    use crate::repo::vulnscan::ScanJob;
    use crate::testing::repo::{
        TestManager, call, mk_format_repo, mk_repo, mux, new_test_manager, spawn_upstream,
    };

    /// Activates the gate; the async `query` is unused where only stored scans are
    /// asserted on.
    pub(crate) struct FakeScanner;

    #[async_trait]
    impl vuln::Scanner for FakeScanner {
        async fn query(&self, _eco: &str, _pkg: &str, _version: &str) -> vuln::Result<Finding> {
            Ok(Finding::default())
        }
        fn source(&self) -> &str {
            "fake"
        }
    }

    /// Records the coordinates it is queried for and returns a clean finding, so a
    /// test can assert which artifacts the backfill scanned.
    #[derive(Default)]
    struct RecordingScanner {
        calls: Mutex<Vec<[String; 3]>>,
    }

    #[async_trait]
    impl vuln::Scanner for RecordingScanner {
        async fn query(&self, eco: &str, pkg: &str, ver: &str) -> vuln::Result<Finding> {
            self.calls
                .lock()
                .push([eco.to_string(), pkg.to_string(), ver.to_string()]);
            Ok(Finding::default())
        }
        fn source(&self) -> &str {
            "rec"
        }
    }

    fn vuln_cfg(action: &str, threshold: &str, ignore: &[&str]) -> Config {
        let mut cfg = repoconfig::default();
        cfg.vuln = VulnPolicyConfig {
            enabled: true,
            action: action.to_string(),
            threshold: threshold.to_string(),
            ignore: ignore.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        cfg
    }

    /// Serves a fixed tarball body, standing in for the npm CDN.
    async fn tarball_upstream() -> String {
        spawn_upstream(Router::new().fallback(|| async { "tarball-bytes" })).await
    }

    /// Drains the scan queue synchronously so assertions do not race the worker.
    async fn drain_scan_queue(m: &Arc<Manager>) {
        let mut rx = m.scan_queue.rx.lock().await;
        let rx = rx.as_mut().expect("queue receiver");
        while let Ok(job) = rx.try_recv() {
            let job: ScanJob = job;
            m.run_scan(job).await;
        }
    }

    #[tokio::test]
    async fn vuln_gate() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager.set_vuln_scanner(Some(Arc::new(FakeScanner)));
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            vuln_cfg(VULN_ACTION_BLOCK, SEVERITY_HIGH, &[]),
        )
        .await;
        let h = mux(&tm.manager);
        let tarball = "/npm/npmjs/lodash/-/lodash-4.17.99.tgz";

        // Critical vuln, block action -> 403.
        upsert_scan(&tm, "critical", &["CVE-2026-1"]).await;
        let resp = call(&h, Method::GET, tarball, "").await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "blocked");
        assert!(
            resp.text().contains("known vulnerabilities"),
            "body = {:?}",
            resp.text()
        );

        // Below threshold (low < high) -> served.
        upsert_scan(&tm, "low", &["CVE-2026-1"]).await;
        let resp = call(&h, Method::GET, tarball, "").await;
        assert_eq!(resp.status, StatusCode::OK, "below-threshold should serve");
    }

    async fn upsert_scan(tm: &TestManager, severity: &str, ids: &[&str]) {
        tm.store
            .upsert_vuln_scan(
                "npm",
                "lodash",
                "4.17.99",
                severity,
                &ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert scan");
    }

    #[tokio::test]
    async fn vuln_gate_ignore_and_audit() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager.set_vuln_scanner(Some(Arc::new(FakeScanner)));

        // Ignore list covers the only advisory -> served despite critical severity.
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            vuln_cfg(VULN_ACTION_BLOCK, SEVERITY_HIGH, &["CVE-2026-1"]),
        )
        .await;
        upsert_scan(&tm, "critical", &["CVE-2026-1"]).await;
        let h = mux(&tm.manager);
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "ignored advisory should serve");

        // Audit mode never blocks even at/above threshold.
        mk_format_repo(
            &tm.store,
            "npm-audit",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            vuln_cfg(VULN_ACTION_AUDIT, SEVERITY_HIGH, &[]),
        )
        .await;
        tm.store
            .upsert_vuln_scan(
                "npm",
                "react",
                "1.0.0",
                "critical",
                &["CVE-2026-2".to_string()],
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert scan");
        let h = mux(&tm.manager);
        let resp = call(
            &h,
            Method::GET,
            "/npm/npm-audit/react/-/react-1.0.0.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "audit mode must serve");
    }

    #[tokio::test]
    async fn vuln_gate_block_unscanned() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager.set_vuln_scanner(Some(Arc::new(FakeScanner)));
        let mut cfg = vuln_cfg(VULN_ACTION_BLOCK, SEVERITY_HIGH, &[]);
        cfg.vuln.block_unscanned = true;
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

        // No stored scan + block_unscanned -> 403 pending.
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/unscanned/-/unscanned-1.0.0.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "block_unscanned");
        assert!(
            resp.text().contains("pending vulnerability scan"),
            "body = {:?}",
            resp.text()
        );
    }

    #[tokio::test]
    async fn vuln_gate_disabled_without_scanner() {
        let upstream = tarball_upstream().await;
        // No scanner set: even a stored critical vuln does not block (feature off).
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            vuln_cfg(VULN_ACTION_BLOCK, SEVERITY_HIGH, &[]),
        )
        .await;
        upsert_scan(&tm, "critical", &["CVE-2026-1"]).await;
        let h = mux(&tm.manager);
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "gate must be off without a scanner"
        );
    }

    /// The backfill enqueues scans for already-stored artifacts that have never been
    /// scanned, and skips ones that already have a stored scan.
    #[tokio::test]
    async fn vuln_backfill_scans_stored_artifacts() {
        let tm = new_test_manager().await;
        let rec = Arc::new(RecordingScanner::default());
        tm.manager.set_vuln_scanner(Some(rec.clone()));

        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            "http://upstream.invalid",
            repoconfig::default(),
        )
        .await;
        let repo = tm
            .store
            .get_repository_by_name("npmjs")
            .await
            .expect("repository");
        for (path, version) in [
            ("lodash/-/lodash-4.17.99.tgz", "4.17.99"),
            ("react/-/react-18.0.0.tgz", "18.0.0"),
        ] {
            tm.store
                .put_artifact(Artifact {
                    repo_id: repo.id,
                    path: path.to_string(),
                    version: version.to_string(),
                    blob_sha256: path.to_string(),
                    size: 4,
                    ..Default::default()
                })
                .await
                .expect("put artifact");
        }
        // "react" is already scanned, "lodash" is not.
        tm.store
            .upsert_vuln_scan(
                "npm",
                "react",
                "18.0.0",
                "none",
                &[],
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert scan");

        tm.manager.backfill_once().await;
        drain_scan_queue(&tm.manager).await;

        // Only the unscanned coordinate was queried, and its scan is now stored.
        let calls = rec.calls.lock().clone();
        assert_eq!(
            calls,
            vec![[
                "npm".to_string(),
                "lodash".to_string(),
                "4.17.99".to_string()
            ]],
            "backfill scanned the wrong set"
        );
        tm.store
            .get_vuln_scan("npm", "lodash", "4.17.99")
            .await
            .expect("lodash scan not stored");
    }

    /// Publishing to a hosted repository enqueues an immediate vulnerability scan
    /// for the uploaded coordinate.
    #[tokio::test]
    async fn hosted_upload_triggers_scan() {
        let tm = new_test_manager().await;
        let rec = Arc::new(RecordingScanner::default());
        tm.manager.set_vuln_scanner(Some(rec.clone()));
        mk_repo(
            &tm.store,
            "mvn",
            meta::TYPE_HOSTED,
            "",
            repoconfig::default(),
        )
        .await;
        let h = mux(&tm.manager);

        let resp = call(
            &h,
            Method::PUT,
            "/maven/mvn/com/example/app/1.2.3/app-1.2.3.jar",
            "JARDATA",
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED, "upload");

        drain_scan_queue(&tm.manager).await;
        let calls = rec.calls.lock().clone();
        assert_eq!(
            calls,
            vec![[
                "Maven".to_string(),
                "com.example:app".to_string(),
                "1.2.3".to_string()
            ]],
        );
        tm.store
            .get_vuln_scan("Maven", "com.example:app", "1.2.3")
            .await
            .expect("scan not stored");
    }

    /// Caching a proxied artifact enqueues a scan even when no vulnerability policy
    /// is enabled: collection is decoupled from enforcement and gated only by a
    /// configured scanner.
    #[tokio::test]
    async fn proxy_fetch_triggers_scan() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        let sc = Arc::new(RecordingScanner::default());
        tm.manager.set_vuln_scanner(Some(sc.clone()));
        // `repoconfig::default()` leaves the vuln policy disabled (no gating).
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

        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "proxy fetch");

        drain_scan_queue(&tm.manager).await;
        let calls = sc.calls.lock().clone();
        assert_eq!(
            calls,
            vec![[
                "npm".to_string(),
                "lodash".to_string(),
                "4.17.99".to_string()
            ]],
        );
    }

    #[tokio::test]
    async fn disabled_repository_refuses_serving() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
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
        let path = "/npm/npmjs/lodash/-/lodash-4.17.21.tgz";

        // Online: served.
        let resp = call(&h, Method::GET, path, "").await;
        assert_eq!(resp.status, StatusCode::OK, "online repo");

        // Disabled: 503, no serving.
        let repo = tm
            .store
            .get_repository_by_name("npmjs")
            .await
            .expect("repository");
        tm.store
            .set_repository_disabled(repo.id, true)
            .await
            .expect("disable repo");
        let resp = call(&h, Method::GET, path, "").await;
        assert_eq!(
            resp.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "disabled repo"
        );

        // Re-enabled: served again.
        tm.store
            .set_repository_disabled(repo.id, false)
            .await
            .expect("enable repo");
        let resp = call(&h, Method::GET, path, "").await;
        assert_eq!(resp.status, StatusCode::OK, "re-enabled repo");
    }
}
