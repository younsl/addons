//! Alert rules: Alertmanager-style schema stored as `AlertRule` custom
//! resources in the `trivy-collector.security.io` API group, with Slack
//! webhook delivery.

pub mod crd;
pub mod evaluator;
pub mod expr;
pub mod migration;
pub mod notifier;
pub mod preview;
pub mod readiness;
pub mod store;
pub mod types;

pub use evaluator::AlertEvaluator;
pub use store::{AlertStore, AlertStoreError};
pub use types::{AlertRule, Matchers, Receiver, SlackReceiver};

use anyhow::Result;
use tracing::{info, warn};

/// Build an evaluator over the in-cluster `AlertRule` resources.
///
/// Both pods call this: the scraper to fire alerts on the ingest path, the
/// server to serve rule CRUD, preview, and test delivery. Every read goes to
/// the API server, so there is nothing to coordinate between them.
pub async fn build_evaluator(external_url: &str) -> Result<AlertEvaluator> {
    let (client, namespace) = crate::kube_env::client_and_namespace().await?;
    let store = AlertStore::new(client.clone(), namespace.clone());
    info!(
        namespace = %namespace,
        resource = %format!("{}.{}", crd::PLURAL, crd::API_GROUP),
        external_url = %external_url,
        "Alerts subsystem enabled"
    );
    // A missing CRD is not fatal: the pod still serves everything else, and
    // the alerts endpoints explain the install-time mistake rather than
    // failing opaquely. Skip the migration in that case, since it has nowhere
    // to write.
    match store.preflight().await {
        Ok(()) => {
            if let Err(e) = migration::run(client, &namespace, &store).await {
                warn!(error = %e, "Legacy alert ConfigMap migration failed; will retry on next start");
            }
        }
        Err(e) => warn!(error = %e, "Alert rules are unavailable"),
    }
    let url = (!external_url.is_empty()).then(|| external_url.to_string());
    Ok(AlertEvaluator::with_external_url(store, url))
}
