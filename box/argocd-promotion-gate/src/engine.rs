//! Gathers the facts a verdict depends on and delegates the verdict itself to
//! the pure rules in the gate module.
//!
//! Both entry points share it: the admission webhook, which enforces, and the
//! UI extension API, which previews. Sharing the engine is what keeps the panel
//! in the Argo CD UI from disagreeing with the denial a user gets on Sync.

use std::sync::Arc;
use std::time::Instant;

use crate::argocd::{AppReader, ImageResolver};
use crate::config::Config;
use crate::gate::{self, AppSnapshot, Decision, Input};
use crate::observability::Metrics;

/// Evaluates the gate for one Application.
pub struct Engine {
    cfg: Config,
    reader: Arc<dyn AppReader>,
    images: Option<Arc<dyn ImageResolver>>,
    metrics: Arc<Metrics>,
}

impl Engine {
    /// Builds an engine. `images` may be `None`, which disables the image
    /// comparison regardless of configuration.
    #[must_use]
    pub fn new(
        cfg: Config,
        reader: Arc<dyn AppReader>,
        images: Option<Arc<dyn ImageResolver>>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            cfg,
            reader,
            images,
            metrics,
        }
    }

    /// The effective configuration.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// The Application reader, so callers can resolve an app by name.
    #[must_use]
    pub fn reader(&self) -> &dyn AppReader {
        self.reader.as_ref()
    }

    /// Gathers upstream state and desired images, then returns the verdict.
    pub async fn evaluate(&self, app: AppSnapshot) -> Decision {
        if !self.cfg.is_gated(&app.project) {
            let verdict = Decision::not_gated(&app.name, &app.project, &app.identity);
            self.metrics.record_decision(&verdict);
            return verdict;
        }

        let upstream_env = self
            .cfg
            .upstream_env(&app.project)
            .unwrap_or_default()
            .to_string();
        let upstream_app = gate::app_name_for(&upstream_env, &app.identity);

        let started = Instant::now();
        let lookup = self.reader.get(&upstream_app).await;
        let result = match &lookup {
            Err(_) => "error",
            Ok(None) => "missing",
            Ok(Some(_)) => "ok",
        };
        self.metrics
            .observe_upstream_lookup(result, started.elapsed());

        let upstream = match lookup {
            Ok(upstream) => upstream,
            Err(err) => {
                // Reporting a failed read as "upstream missing" would silently
                // open the gate, so it stays a lookup failure and the onError
                // policy decides.
                tracing::warn!(app = %app.name, upstream = %upstream_app, error = %err, "upstream application lookup failed");
                self.metrics.record_lookup_failure("upstream");
                let input = Input {
                    app,
                    lookup_error: err.to_string(),
                    ..Input::default()
                };
                let verdict = gate::with_upstream(
                    gate::evaluate(&input, &self.cfg),
                    &upstream_env,
                    &upstream_app,
                    None,
                );
                self.metrics.record_decision(&verdict);
                return verdict;
            }
        };

        let mut input = Input {
            app,
            upstream,
            ..Input::default()
        };

        // The desired image lookup is the only remote call, so it is skipped
        // whenever the verdict cannot depend on it: an upstream that already
        // fails the sync or health check denies before images matter.
        if let Some(resolver) = self.needs_images(input.upstream.as_ref(), &input.app) {
            let started = Instant::now();
            match resolver.desired_images(&input.app.name).await {
                Ok(images) => {
                    self.metrics.observe_desired_images("ok", started.elapsed());
                    input.desired_images = Some(images);
                }
                Err(err) => {
                    self.metrics
                        .observe_desired_images("error", started.elapsed());
                    tracing::warn!(app = %input.app.name, error = %err, "desired image lookup failed");
                    self.metrics.record_lookup_failure("desired_images");
                    input.lookup_error = err.to_string();
                }
            }
        }

        let upstream_snapshot = input.upstream.clone();
        let verdict = gate::with_upstream(
            gate::evaluate(&input, &self.cfg),
            &upstream_env,
            &upstream_app,
            upstream_snapshot.as_ref(),
        );
        self.metrics.record_decision(&verdict);
        verdict
    }

    fn needs_images(
        &self,
        upstream: Option<&AppSnapshot>,
        app: &AppSnapshot,
    ) -> Option<&dyn ImageResolver> {
        if !self.cfg.image_tag.enabled || app.skip_requested {
            return None;
        }
        let upstream = upstream?;
        if self.cfg.require.sync && !upstream.is_synced() {
            return None;
        }
        if self.cfg.require.health && !upstream.is_healthy() {
            return None;
        }
        self.images.as_deref()
    }
}

#[cfg(test)]
pub mod testing {
    //! In-memory doubles shared by the engine and the HTTP handler tests.

    use std::collections::HashMap;

    use async_trait::async_trait;

    use crate::argocd::api::ApiError;
    use crate::argocd::application::ReadError;
    use crate::argocd::application::SnapshotError;
    use crate::argocd::{AppReader, ImageResolver};
    use crate::gate::{AppSnapshot, ImageRef, identity_of, parse_image};

    /// An [`AppReader`] over a fixed set of snapshots, optionally failing.
    #[derive(Default)]
    pub struct FakeReader {
        pub apps: HashMap<String, AppSnapshot>,
        pub fail: bool,
    }

    impl FakeReader {
        pub fn with(apps: Vec<AppSnapshot>) -> Self {
            Self {
                apps: apps.into_iter().map(|a| (a.name.clone(), a)).collect(),
                fail: false,
            }
        }
    }

    #[async_trait]
    impl AppReader for FakeReader {
        async fn get(&self, name: &str) -> Result<Option<AppSnapshot>, ReadError> {
            if self.fail {
                return Err(ReadError::Snapshot(SnapshotError::MissingName));
            }
            Ok(self.apps.get(name).cloned())
        }

        async fn list(&self) -> Result<Vec<AppSnapshot>, ReadError> {
            if self.fail {
                return Err(ReadError::Snapshot(SnapshotError::MissingName));
            }
            Ok(self.apps.values().cloned().collect())
        }
    }

    /// An [`ImageResolver`] that returns a fixed answer.
    pub struct FakeResolver {
        pub images: Result<Vec<ImageRef>, String>,
    }

    impl FakeResolver {
        pub fn returning(images: &[&str]) -> Self {
            Self {
                images: Ok(images.iter().map(|i| parse_image(i)).collect()),
            }
        }

        pub fn failing(message: &str) -> Self {
            Self {
                images: Err(message.to_string()),
            }
        }
    }

    #[async_trait]
    impl ImageResolver for FakeResolver {
        async fn desired_images(&self, _app: &str) -> Result<Vec<ImageRef>, ApiError> {
            self.images.clone().map_err(ApiError::NoToken)
        }
    }

    pub fn snapshot(
        name: &str,
        project: &str,
        sync: &str,
        health: &str,
        images: &[&str],
    ) -> AppSnapshot {
        AppSnapshot {
            name: name.to_string(),
            project: project.to_string(),
            identity: identity_of(name, project),
            sync_status: sync.to_string(),
            health_status: health.to_string(),
            live_images: images.iter().map(|i| parse_image(i)).collect(),
            ..AppSnapshot::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeReader, FakeResolver, snapshot};
    use super::*;
    use crate::gate::Code;

    fn engine(
        cfg: &str,
        reader: FakeReader,
        resolver: Option<FakeResolver>,
    ) -> (Engine, Arc<Metrics>) {
        let metrics = Arc::new(Metrics::new());
        let cfg = Config::parse(&format!("chain: [stg, prd]\n{cfg}")).unwrap();
        let images = resolver.map(|r| Arc::new(r) as Arc<dyn ImageResolver>);
        (
            Engine::new(cfg, Arc::new(reader), images, Arc::clone(&metrics)),
            metrics,
        )
    }

    #[tokio::test]
    async fn not_gated_never_reads_anything() {
        let (eng, metrics) = engine(
            "",
            FakeReader {
                fail: true,
                ..FakeReader::default()
            },
            None,
        );
        let verdict = eng.evaluate(snapshot("stg-api", "stg", "", "", &[])).await;
        assert_eq!(verdict.code, Code::NotGated);
        assert!(metrics.encode().unwrap().contains(r#"code="NotGated""#));
        assert_eq!(eng.config().chain, vec!["stg", "prd"]);
        assert!(eng.reader().get("x").await.is_err());
    }

    #[tokio::test]
    async fn kubernetes_failure_is_not_a_missing_upstream() {
        let resolver = FakeResolver::returning(&["r/api:1"]);
        let (eng, metrics) = engine(
            "",
            FakeReader {
                fail: true,
                ..FakeReader::default()
            },
            Some(resolver),
        );
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(!verdict.allowed);
        let up = verdict.upstream.unwrap();
        assert!(!up.exists);
        assert_eq!(up.app, "stg-api");
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"lookup_failures_total{kind="upstream"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"upstream_lookup_duration_seconds_count{result="error"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn missing_upstream_is_allowed_and_skips_images() {
        let resolver = FakeResolver::returning(&["r/api:1"]);
        let (eng, metrics) = engine("", FakeReader::default(), Some(resolver));
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::UpstreamMissing);
        assert!(verdict.allowed);
        assert!(metrics.encode().unwrap().contains(r#"result="missing""#));
    }

    #[tokio::test]
    async fn skips_image_lookup_when_upstream_already_fails() {
        let upstream = snapshot("stg-api", "stg", "OutOfSync", "Healthy", &["r/api:1"]);
        let (eng, _) = engine(
            "",
            FakeReader::with(vec![upstream]),
            Some(FakeResolver::returning(&["r/api:2"])),
        );
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::UpstreamOutOfSync);
        let up = verdict.upstream.unwrap();
        assert!(up.exists);
        assert_eq!(up.sync_status, "OutOfSync");
    }

    #[tokio::test]
    async fn resolver_is_skipped_for_exempt_apps_and_unhealthy_upstreams() {
        let upstream = snapshot("stg-api", "stg", "Synced", "Degraded", &["r/api:1"]);
        let (eng, _) = engine(
            "",
            FakeReader::with(vec![upstream]),
            Some(FakeResolver::returning(&["r/api:2"])),
        );
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::UpstreamUnhealthy);

        let mut skip = snapshot("prd-api", "prd", "", "", &[]);
        skip.skip_requested = true;
        let verdict = eng.evaluate(skip).await;
        assert_eq!(verdict.code, Code::Exempt);
    }

    #[tokio::test]
    async fn compares_images_when_upstream_is_ready() {
        let upstream = snapshot("stg-api", "stg", "Synced", "Healthy", &["r/api:1"]);
        let (eng, metrics) = engine(
            "imageTag:\n  mode: enforce\n",
            FakeReader::with(vec![upstream]),
            Some(FakeResolver::returning(&["r/api:2"])),
        );
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::ImageTagMismatch);
        assert!(!verdict.allowed);
        assert_eq!(verdict.images.len(), 1);
        assert!(
            metrics
                .encode()
                .unwrap()
                .contains(r#"desired_images_duration_seconds_count{result="ok"} 1"#)
        );
    }

    #[tokio::test]
    async fn resolver_failure_becomes_lookup_failed() {
        let upstream = snapshot("stg-api", "stg", "Synced", "Healthy", &["r/api:1"]);
        let (eng, metrics) = engine(
            "imageTag:\n  onError: allow\n",
            FakeReader::with(vec![upstream]),
            Some(FakeResolver::failing("/token")),
        );
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(verdict.code, Code::LookupFailed);
        assert!(verdict.allowed);
        assert!(verdict.warnings[0].contains("/token"));
        let text = metrics.encode().unwrap();
        assert!(
            text.contains(r#"lookup_failures_total{kind="desired_images"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn no_resolver_means_no_comparison() {
        let upstream = snapshot("stg-api", "stg", "Synced", "Healthy", &["r/api:1"]);
        let (eng, _) = engine("", FakeReader::with(vec![upstream]), None);
        let verdict = eng.evaluate(snapshot("prd-api", "prd", "", "", &[])).await;
        assert_eq!(
            verdict.code,
            Code::LookupFailed,
            "image tag enabled without a resolver is a failed lookup"
        );
    }
}
