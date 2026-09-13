//! Gates a coordinate on its resolved SPDX license(s).

use std::collections::HashSet;
use std::sync::Arc;

use http::request::Parts;
use http::{Method, StatusCode};

use crate::repoconfig::{VULN_ACTION_AUDIT, VULN_ACTION_BLOCK, VULN_ACTION_WARN};
use crate::server::http_error;
use crate::{audit, auth, meta};

use super::licensescan::deps_dev_system;
use super::router::HandlerFutureOpt;
use super::{Manager, Resolved};

impl Manager {
    /// Enforces the per-repository license policy for proxy and hosted reads. It
    /// consults stored resolution results only (never blocking on a live
    /// lookup): a not-yet-resolved coordinate is queued for async resolution
    /// and, unless `block_unresolved` is set, served meanwhile. A resolved
    /// coordinate whose licenses violate the policy (a denied license, or a
    /// license outside a non-empty allow list) is blocked, warned, or audited
    /// per `action`.
    ///
    /// `Some(response)` means the request was blocked.
    pub(crate) fn license_gate(
        m: Arc<Manager>,
        parts: Arc<Parts>,
        res: Arc<Resolved>,
        pkg: String,
        version: String,
    ) -> HandlerFutureOpt {
        Box::pin(async move {
            if m.resolver.read().is_none()
                || (res.repo.r#type != meta::TYPE_PROXY && res.repo.r#type != meta::TYPE_HOSTED)
            {
                return None;
            }
            if parts.method != Method::GET && parts.method != Method::HEAD {
                return None;
            }
            let cfg = &res.cfg.license;
            if !cfg.enabled || pkg.is_empty() || version.is_empty() {
                return None;
            }
            let system = deps_dev_system(&res.repo.format);
            if system.is_empty() {
                return None;
            }

            let scan = match m.store.get_license_scan(system, &pkg, &version).await {
                Ok(scan) => scan,
                Err(meta::Error::NotFound) => {
                    m.enqueue_resolve(system, &pkg, &version);
                    // Unknown coordinate: fail open unless the policy opts into
                    // blocking pending resolutions under an enforcing posture.
                    if cfg.effective_action() == VULN_ACTION_BLOCK && cfg.block_unresolved {
                        return Some(http_error(
                            StatusCode::FORBIDDEN,
                            &format!("package pending license resolution: {pkg}"),
                        ));
                    }
                    return None;
                }
                Err(err) => {
                    // Best-effort: a lookup error must not break serving.
                    tracing::error!(
                        repo = %res.repo.name, package = %pkg, version = %version, err = %err,
                        "license lookup failed"
                    );
                    return None;
                }
            };

            let reason = license_violation(&scan.licenses, &cfg.deny, &cfg.allow)?;

            let action = cfg.effective_action();
            m.license_blocked
                .with_label_values(&[&res.repo.name, action])
                .inc();
            let licenses = scan.licenses.join(",");
            if action == VULN_ACTION_AUDIT || action == VULN_ACTION_WARN {
                tracing::warn!(
                    repo = %res.repo.name, package = %pkg, version = %version,
                    licenses = %licenses, reason = %reason, action,
                    "license policy: would block"
                );
                return None;
            }

            if let Some(rec) = &m.rec {
                let username = auth::from_request_parts(&parts)
                    .map(|p| p.username.clone())
                    .unwrap_or_default();
                rec.record(audit::Event {
                    repo: res.repo.name.clone(),
                    action: meta::EVENT_LICENSE_BLOCK.to_string(),
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
                licenses = %licenses, reason = %reason,
                "package blocked by license policy"
            );
            Some(http_error(
                StatusCode::FORBIDDEN,
                &format!("blocked: license policy ({reason}) for {pkg}@{version}"),
            ))
        })
    }
}

/// Reports the human-readable reason a coordinate's licenses violate the policy,
/// or `None` when they do not. A license in `deny` always violates; if `allow`
/// is non-empty, any license outside `allow` violates (allow-list mode).
/// Coordinates with no resolved license never violate here (the
/// `block_unresolved` path governs the unknown case). Matching is
/// case-insensitive on the SPDX identifier.
pub(crate) fn license_violation(
    licenses: &[String],
    deny: &[String],
    allow: &[String],
) -> Option<String> {
    let deny_set = lower_set(deny);
    for l in licenses {
        if deny_set.contains(&l.to_lowercase()) {
            return Some(format!("denied license {l}"));
        }
    }
    if !allow.is_empty() && !licenses.is_empty() {
        let allow_set = lower_set(allow);
        for l in licenses {
            if !allow_set.contains(&l.to_lowercase()) {
                return Some(format!("license {l} not in allow list"));
            }
        }
    }
    None
}

fn lower_set(items: &[String]) -> HashSet<String> {
    items
        .iter()
        .map(|it| it.trim())
        .filter(|it| !it.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::Router;
    use http::{Method, StatusCode};
    use parking_lot::Mutex;

    use crate::license::{self, LicenseResult};
    use crate::meta;
    use crate::repoconfig::{
        self, Config, LicensePolicyConfig, VULN_ACTION_AUDIT, VULN_ACTION_BLOCK,
    };

    use crate::repo::Manager;
    use crate::repo::licensegate::license_violation;
    use crate::repo::licensescan::ResolveJob;
    use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, spawn_upstream};

    /// Activates the gate; the async `resolve` is unused where only stored results
    /// are asserted on.
    pub(crate) struct FakeResolver;

    #[async_trait]
    impl license::Resolver for FakeResolver {
        async fn resolve(
            &self,
            _system: &str,
            _pkg: &str,
            _v: &str,
        ) -> license::Result<LicenseResult> {
            Ok(LicenseResult::default())
        }
        fn source(&self) -> &str {
            "fake"
        }
    }

    /// Records the coordinates it resolves, for backfill/upload tests.
    #[derive(Default)]
    struct RecordingResolver {
        calls: Mutex<Vec<[String; 3]>>,
    }

    #[async_trait]
    impl license::Resolver for RecordingResolver {
        async fn resolve(
            &self,
            system: &str,
            pkg: &str,
            ver: &str,
        ) -> license::Result<LicenseResult> {
            self.calls
                .lock()
                .push([system.to_string(), pkg.to_string(), ver.to_string()]);
            Ok(LicenseResult {
                licenses: vec!["MIT".to_string()],
            })
        }
        fn source(&self) -> &str {
            "rec"
        }
    }

    fn license_cfg(action: &str, deny: &[&str], allow: &[&str]) -> Config {
        let mut cfg = repoconfig::default();
        cfg.license = LicensePolicyConfig {
            enabled: true,
            action: action.to_string(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        cfg
    }

    async fn tarball_upstream() -> String {
        spawn_upstream(Router::new().fallback(|| async { "tarball-bytes" })).await
    }

    /// Drains the resolve queue synchronously so assertions do not race the worker.
    async fn drain_resolve_queue(m: &Arc<Manager>) -> usize {
        let mut rx = m.resolve_queue.rx.lock().await;
        let rx = rx.as_mut().expect("queue receiver");
        let mut drained = 0;
        while let Ok(job) = rx.try_recv() {
            let job: ResolveJob = job;
            m.run_resolve(job).await;
            drained += 1;
        }
        drained
    }

    #[tokio::test]
    async fn license_gate() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager
            .set_license_resolver(Some(Arc::new(FakeResolver)));
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            license_cfg(VULN_ACTION_BLOCK, &["GPL-3.0"], &[]),
        )
        .await;
        let h = mux(&tm.manager);
        let tarball = "/npm/npmjs/copyleft/-/copyleft-1.0.0.tgz";

        // Denied license (case-insensitive match) -> 403.
        tm.store
            .upsert_license_scan(
                "npm",
                "copyleft",
                "1.0.0",
                &["gpl-3.0".to_string()],
                "deps.dev",
            )
            .await
            .expect("upsert license scan");
        let resp = call(&h, Method::GET, tarball, "").await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "denied license");
        assert!(
            resp.text().contains("license policy"),
            "body = {:?}",
            resp.text()
        );

        // Permissive license, not denied -> served.
        tm.store
            .upsert_license_scan("npm", "copyleft", "1.0.0", &["MIT".to_string()], "deps.dev")
            .await
            .expect("upsert license scan");
        let resp = call(&h, Method::GET, tarball, "").await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "permissive license should serve"
        );
    }

    #[tokio::test]
    async fn license_gate_allow_list_and_audit() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager
            .set_license_resolver(Some(Arc::new(FakeResolver)));

        // Allow-list mode: a license outside the allow list is blocked.
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            license_cfg(VULN_ACTION_BLOCK, &[], &["MIT", "Apache-2.0"]),
        )
        .await;
        tm.store
            .upsert_license_scan(
                "npm",
                "weird",
                "1.0.0",
                &["BSD-3-Clause".to_string()],
                "deps.dev",
            )
            .await
            .expect("upsert license scan");
        let h = mux(&tm.manager);
        let resp = call(&h, Method::GET, "/npm/npmjs/weird/-/weird-1.0.0.tgz", "").await;
        assert_eq!(
            resp.status,
            StatusCode::FORBIDDEN,
            "license outside allow list should block"
        );

        // Allowed license -> served.
        tm.store
            .upsert_license_scan("npm", "ok", "1.0.0", &["MIT".to_string()], "deps.dev")
            .await
            .expect("upsert license scan");
        let h = mux(&tm.manager);
        let resp = call(&h, Method::GET, "/npm/npmjs/ok/-/ok-1.0.0.tgz", "").await;
        assert_eq!(resp.status, StatusCode::OK, "allowed license should serve");

        // Audit mode never blocks even on a denied license.
        mk_format_repo(
            &tm.store,
            "npm-audit",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            license_cfg(VULN_ACTION_AUDIT, &["GPL-3.0"], &[]),
        )
        .await;
        tm.store
            .upsert_license_scan(
                "npm",
                "copyleft",
                "2.0.0",
                &["GPL-3.0".to_string()],
                "deps.dev",
            )
            .await
            .expect("upsert license scan");
        let h = mux(&tm.manager);
        let resp = call(
            &h,
            Method::GET,
            "/npm/npm-audit/copyleft/-/copyleft-2.0.0.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "audit mode must serve");
    }

    #[tokio::test]
    async fn license_gate_block_unresolved() {
        let upstream = tarball_upstream().await;
        let tm = new_test_manager().await;
        tm.manager
            .set_license_resolver(Some(Arc::new(FakeResolver)));
        let mut cfg = license_cfg(VULN_ACTION_BLOCK, &["GPL-3.0"], &[]);
        cfg.license.block_unresolved = true;
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

        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/unknown/-/unknown-1.0.0.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "block_unresolved");
        assert!(
            resp.text().contains("pending license resolution"),
            "body = {:?}",
            resp.text()
        );
    }

    #[tokio::test]
    async fn license_gate_disabled_without_resolver() {
        let upstream = tarball_upstream().await;
        // No resolver set: even a stored denied license does not block (feature off).
        let tm = new_test_manager().await;
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            license_cfg(VULN_ACTION_BLOCK, &["GPL-3.0"], &[]),
        )
        .await;
        tm.store
            .upsert_license_scan(
                "npm",
                "copyleft",
                "1.0.0",
                &["GPL-3.0".to_string()],
                "deps.dev",
            )
            .await
            .expect("upsert license scan");
        let h = mux(&tm.manager);
        let resp = call(
            &h,
            Method::GET,
            "/npm/npmjs/copyleft/-/copyleft-1.0.0.tgz",
            "",
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::OK,
            "gate must be off without a resolver"
        );
    }

    /// One `licenseViolation` table row: name, licenses, deny, allow, expectation.
    type ViolationCase = (
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
        &'static [&'static str],
        bool,
    );

    #[test]
    fn license_violation_cases() {
        let cases: &[ViolationCase] = &[
            ("clean", &["MIT"], &["GPL-3.0"], &[], false),
            ("denied", &["GPL-3.0"], &["GPL-3.0"], &[], true),
            (
                "denied case-insensitive",
                &["gpl-3.0"],
                &["GPL-3.0"],
                &[],
                true,
            ),
            ("allow ok", &["MIT"], &[], &["MIT", "Apache-2.0"], false),
            ("allow miss", &["BSD-3-Clause"], &[], &["MIT"], true),
            (
                "allow all present",
                &["MIT", "Apache-2.0"],
                &[],
                &["MIT", "Apache-2.0"],
                false,
            ),
            (
                "allow one missing",
                &["MIT", "GPL-3.0"],
                &[],
                &["MIT"],
                true,
            ),
            (
                "empty licenses never violate",
                &[],
                &["GPL-3.0"],
                &["MIT"],
                false,
            ),
        ];
        for (name, licenses, deny, allow, want) in cases {
            let to_vec = |v: &&[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let got = license_violation(&to_vec(licenses), &to_vec(deny), &to_vec(allow)).is_some();
            assert_eq!(got, *want, "{name}");
        }
    }

    /// Publishing to a hosted repository enqueues an immediate license resolution
    /// for the uploaded coordinate.
    #[tokio::test]
    async fn hosted_upload_triggers_resolve() {
        let tm = new_test_manager().await;
        let rec = Arc::new(RecordingResolver::default());
        tm.manager.set_license_resolver(Some(rec.clone()));
        mk_format_repo(
            &tm.store,
            "mvn",
            meta::FORMAT_MAVEN,
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

        drain_resolve_queue(&tm.manager).await;
        let calls = rec.calls.lock().clone();
        assert_eq!(
            calls,
            vec![[
                "maven".to_string(),
                "com.example:app".to_string(),
                "1.2.3".to_string()
            ]],
        );
        tm.store
            .get_license_scan("maven", "com.example:app", "1.2.3")
            .await
            .expect("license result not stored");
    }
}
