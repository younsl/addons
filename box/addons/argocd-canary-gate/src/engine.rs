//! Gathers the facts a verdict depends on and delegates the verdict itself to
//! the pure rules in the gate module.

use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::gate::{self, AppSnapshot, Decision};
use crate::k8s::RolloutReader;
use crate::observability::Metrics;

/// Evaluates the gate for one Application.
pub struct Engine {
    cfg: Config,
    reader: Arc<dyn RolloutReader>,
    metrics: Arc<Metrics>,
}

impl Engine {
    /// Builds an engine.
    #[must_use]
    pub fn new(cfg: Config, reader: Arc<dyn RolloutReader>, metrics: Arc<Metrics>) -> Self {
        Self {
            cfg,
            reader,
            metrics,
        }
    }

    /// The effective configuration.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// Lists this Application's Rollouts, then returns the verdict.
    pub async fn evaluate(&self, app: AppSnapshot) -> Decision {
        // An exempt Application never costs a Rollout list.
        if app.skip_requested {
            let verdict = gate::decide(&app, &[], &self.cfg);
            self.metrics.record_decision(&verdict, self.cfg.mode);
            return verdict;
        }

        let selector = format!("{}={}", self.cfg.rollouts.tracking_label, app.name);
        let started = Instant::now();
        let lookup = self.reader.list(&app.dest_namespace, &selector).await;
        let result = if lookup.is_ok() { "ok" } else { "error" };
        self.metrics
            .observe_rollout_lookup(result, started.elapsed());

        let verdict = match lookup {
            Ok(rollouts) => gate::decide(&app, &rollouts, &self.cfg),
            Err(err) => {
                tracing::warn!(app = %app.name, selector = %selector, error = %err, "rollout listing failed");
                self.metrics.record_lookup_failure("rollouts");
                gate::lookup_failed(&app, &err.to_string(), &self.cfg)
            }
        };
        self.metrics.record_decision(&verdict, self.cfg.mode);
        verdict
    }
}

#[cfg(test)]
pub mod testing {
    //! In-memory doubles shared by the engine and the HTTP handler tests.

    use std::sync::Mutex;

    use async_trait::async_trait;

    use crate::gate::RolloutSnapshot;
    use crate::k8s::{ReadError, RolloutReader};

    /// A [`RolloutReader`] over a fixed set of snapshots, optionally failing.
    /// It records the namespace and selector of every call.
    #[derive(Default)]
    pub struct FakeReader {
        pub rollouts: Vec<RolloutSnapshot>,
        pub fail: bool,
        pub calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeReader {
        pub fn with(rollouts: Vec<RolloutSnapshot>) -> Self {
            Self {
                rollouts,
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl RolloutReader for FakeReader {
        async fn list(
            &self,
            namespace: &str,
            label_selector: &str,
        ) -> Result<Vec<RolloutSnapshot>, ReadError> {
            self.calls
                .lock()
                .unwrap()
                .push((namespace.to_string(), label_selector.to_string()));
            if self.fail {
                return Err(ReadError::List {
                    selector: label_selector.to_string(),
                    source: kube::Error::Api(Box::new(kube::core::Status::failure(
                        "forbidden",
                        "Forbidden",
                    ))),
                });
            }
            Ok(self.rollouts.clone())
        }
    }

    pub fn rollout(name: &str, stable: &str, current: &str) -> RolloutSnapshot {
        RolloutSnapshot {
            name: name.to_string(),
            namespace: "payments".to_string(),
            strategy: "canary".to_string(),
            stable_hash: stable.to_string(),
            current_hash: current.to_string(),
            ..RolloutSnapshot::default()
        }
    }

    pub fn app(name: &str) -> crate::gate::AppSnapshot {
        crate::gate::AppSnapshot {
            name: name.to_string(),
            dest_namespace: "payments".to_string(),
            skip_requested: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::testing::{FakeReader, app, rollout};
    use super::*;
    use crate::gate::Code;

    fn engine(cfg: &str, reader: FakeReader) -> (Engine, Arc<FakeReader>, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::new());
        let cfg = Config::parse(cfg).unwrap();
        let reader = Arc::new(reader);
        (
            Engine::new(
                cfg,
                Arc::clone(&reader) as Arc<dyn RolloutReader>,
                Arc::clone(&metrics),
            ),
            reader,
            metrics,
        )
    }

    #[tokio::test]
    async fn exempt_never_lists_anything() {
        let (eng, reader, metrics) = engine(
            "{}",
            FakeReader {
                fail: true,
                ..FakeReader::default()
            },
        );
        let mut exempt = app("prd-api");
        exempt.skip_requested = true;
        let verdict = eng.evaluate(exempt).await;
        assert_eq!(verdict.code, Code::Exempt);
        assert!(reader.calls.lock().unwrap().is_empty());
        assert!(metrics.encode().unwrap().contains(r#"code="Exempt""#));
        assert_eq!(eng.config().argocd.namespace, "argocd");
    }

    #[tokio::test]
    async fn lists_with_the_tracking_label_in_the_destination_namespace() {
        let (eng, reader, _) = engine("{}", FakeReader::with(vec![rollout("api", "a", "a")]));
        let verdict = eng.evaluate(app("prd-api")).await;
        assert_eq!(verdict.code, Code::Passed);
        let calls = reader.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[(
                "payments".to_string(),
                "argocd.argoproj.io/instance=prd-api".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn mid_canary_denies_and_counts() {
        let (eng, _, metrics) = engine("{}", FakeReader::with(vec![rollout("api", "a", "b")]));
        let verdict = eng.evaluate(app("prd-api")).await;
        assert_eq!(verdict.code, Code::CanaryInProgress);
        assert!(!verdict.allowed);
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"code="CanaryInProgress",allowed="false",mode="enforce""#),
            "{text}"
        );
        assert!(
            text.contains(r#"rollout_lookup_duration_seconds_count{result="ok"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn list_failure_is_not_no_rollouts() {
        let (eng, _, metrics) = engine(
            "{}",
            FakeReader {
                fail: true,
                ..FakeReader::default()
            },
        );
        let verdict = eng.evaluate(app("prd-api")).await;
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(!verdict.allowed);
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"lookup_failures_total{kind="rollouts"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"rollout_lookup_duration_seconds_count{result="error"} 1"#),
            "{text}"
        );
    }
}
