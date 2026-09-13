//! Orders the configurable policy gates around the fixed age and approval
//! phases.

use std::collections::HashMap;
use std::sync::Arc;

use axum::response::Response;
use http::request::Parts;

use crate::repoconfig::{POLICY_AGE, POLICY_LICENSE, POLICY_VULNERABILITY};

use super::{FinalGate, Manager, Resolved};

/// Adapts the existing policy gates while the pipeline owns their ordering. A
/// `Some(response)` result means the gate answered the request and evaluation
/// stops.
pub(crate) type RequestPolicy =
    fn(Arc<Manager>, Arc<Parts>, Arc<Resolved>, String, String) -> super::router::HandlerFutureOpt;

impl Manager {
    fn policy_registry() -> HashMap<&'static str, RequestPolicy> {
        HashMap::from([
            (POLICY_VULNERABILITY, Manager::vuln_gate as RequestPolicy),
            (POLICY_LICENSE, Manager::license_gate as RequestPolicy),
        ])
    }

    /// Evaluates the configured steps before age. Encountering age transfers
    /// execution to the artifact engine, which has publication metadata.
    pub(crate) async fn policy_gates(
        self: &Arc<Self>,
        parts: Arc<Parts>,
        res: Arc<Resolved>,
        pkg: &str,
        version: &str,
    ) -> Option<Response> {
        if let Some(resp) = Manager::version_deny_gate(
            Arc::clone(self),
            Arc::clone(&parts),
            Arc::clone(&res),
            pkg.to_string(),
            version.to_string(),
        )
        .await
        {
            return Some(resp);
        }
        let registry = Manager::policy_registry();
        for name in res.cfg.policy_pipeline.effective_order() {
            if name == POLICY_AGE {
                break;
            }
            let Some(policy) = registry.get(name.as_str()) else {
                continue;
            };
            if let Some(resp) = policy(
                Arc::clone(self),
                Arc::clone(&parts),
                Arc::clone(&res),
                pkg.to_string(),
                version.to_string(),
            )
            .await
            {
                return Some(resp);
            }
        }
        None
    }

    /// Resumes the configured pipeline after age and always runs human approval
    /// last.
    pub(crate) fn final_policy_gate(
        self: &Arc<Self>,
        res: Resolved,
        pkg: &str,
        version: &str,
    ) -> FinalGate {
        let manager = Arc::clone(self);
        let res = Arc::new(res);
        let pkg = pkg.to_string();
        let version = version.to_string();
        Arc::new(move |parts: Arc<Parts>| {
            let manager = Arc::clone(&manager);
            let res = Arc::clone(&res);
            let pkg = pkg.clone();
            let version = version.clone();
            Box::pin(async move {
                let registry = Manager::policy_registry();
                let mut after_age = false;
                for name in res.cfg.policy_pipeline.effective_order() {
                    if name == POLICY_AGE {
                        after_age = true;
                        continue;
                    }
                    if !after_age {
                        continue;
                    }
                    let Some(policy) = registry.get(name.as_str()) else {
                        continue;
                    };
                    if let Some(resp) = policy(
                        Arc::clone(&manager),
                        Arc::clone(&parts),
                        Arc::clone(&res),
                        pkg.clone(),
                        version.clone(),
                    )
                    .await
                    {
                        return Some(resp);
                    }
                }
                Manager::approval_gate(manager, parts, res, pkg, version).await
            })
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::Router;
    use axum::response::IntoResponse;
    use http::{Method, StatusCode};

    use crate::meta;
    use crate::repoconfig::{
        ACTION_BLOCK, AgePolicyConfig, Duration, MODE_ENFORCE, POLICY_AGE, POLICY_LICENSE,
        POLICY_VULNERABILITY, SEVERITY_HIGH, VULN_ACTION_BLOCK, VulnPolicyConfig,
    };

    use crate::repo::approvalgate::tests::approval_cfg;
    use crate::repo::vulngate::tests::FakeScanner;
    use crate::testing::repo::{call, mk_format_repo, mux, new_test_manager, spawn_upstream};

    #[tokio::test]
    async fn policy_pipeline_configured_order() {
        let upstream = spawn_upstream(Router::new().fallback(|| async { "tarball-bytes" })).await;
        let tm = new_test_manager().await;
        tm.manager.set_vuln_scanner(Some(Arc::new(FakeScanner)));
        let mut cfg = approval_cfg(MODE_ENFORCE, &[]);
        cfg.vuln = VulnPolicyConfig {
            enabled: true,
            action: VULN_ACTION_BLOCK.to_string(),
            threshold: SEVERITY_HIGH.to_string(),
            ..Default::default()
        };
        cfg.policy_pipeline.order = vec![
            POLICY_AGE.to_string(),
            POLICY_VULNERABILITY.to_string(),
            POLICY_LICENSE.to_string(),
        ];
        mk_format_repo(
            &tm.store,
            "npmjs",
            meta::FORMAT_NPM,
            meta::TYPE_PROXY,
            &upstream,
            cfg,
        )
        .await;
        tm.store
            .upsert_vuln_scan(
                "npm",
                "lodash",
                "4.17.99",
                "critical",
                &["CVE-2026-1".to_string()],
                &HashMap::new(),
                0,
                &[],
                "OSV",
            )
            .await
            .expect("upsert vuln scan");

        let app = mux(&tm.manager);
        let resp = call(
            &app,
            Method::GET,
            "/npm/npmjs/lodash/-/lodash-4.17.99.tgz",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
        assert!(
            resp.text().contains("known vulnerabilities"),
            "first policy did not block: {}",
            resp.text()
        );
        let rows = tm
            .store
            .list_approvals("npmjs", meta::APPROVAL_PENDING, 10, 0)
            .await
            .expect("list approvals");
        assert!(
            rows.is_empty(),
            "final approval ran after an automated block: {rows:?}"
        );
    }

    #[tokio::test]
    async fn policy_pipeline_age_runs_before_final_approval() {
        let upstream = spawn_upstream(Router::new().fallback(|| async {
            (
                [(
                    http::header::LAST_MODIFIED,
                    crate::testing::repo::http_time(chrono::Utc::now()),
                )],
                "artifact",
            )
                .into_response()
        }))
        .await;
        let tm = new_test_manager().await;
        let mut cfg = approval_cfg(MODE_ENFORCE, &[]);
        cfg.age_policy = AgePolicyConfig {
            enabled: true,
            min_age: Duration::from_std(std::time::Duration::from_secs(24 * 60 * 60)),
            action: ACTION_BLOCK.to_string(),
            ..Default::default()
        };
        mk_format_repo(
            &tm.store,
            "maven",
            meta::FORMAT_MAVEN,
            meta::TYPE_PROXY,
            &upstream,
            cfg,
        )
        .await;

        let app = mux(&tm.manager);
        let resp = call(
            &app,
            Method::GET,
            "/maven/maven/org/example/lib/1.0/lib-1.0.jar",
            "",
        )
        .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
        assert!(
            resp.text().contains("age policy"),
            "age policy did not block first: {}",
            resp.text()
        );
        let rows = tm
            .store
            .list_approvals("maven", meta::APPROVAL_PENDING, 10, 0)
            .await
            .expect("list approvals");
        assert!(
            rows.is_empty(),
            "final approval ran after age block: {rows:?}"
        );
    }
}
