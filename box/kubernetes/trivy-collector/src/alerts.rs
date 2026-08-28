//! Alert rules: Alertmanager-style schema, ConfigMap-backed storage,
//! Slack webhook delivery.

pub mod evaluator;
pub mod expr;
pub mod notifier;
pub mod preview;
pub mod store;
pub mod types;

pub use evaluator::AlertEvaluator;
pub use store::{AlertStore, AlertStoreError};
pub use types::{AlertRule, Matchers, Receiver, SlackReceiver};

use anyhow::Result;
use tracing::{info, warn};

/// Name of the ConfigMap that holds every alert rule.
pub const ALERTS_CONFIGMAP_NAME: &str = "trivy-collector-alerts";

/// Build an evaluator against the in-cluster alerts ConfigMap.
///
/// Both pods call this: the scraper to fire alerts on the ingest path, the
/// server to serve rule CRUD, preview, and test delivery. `AlertStore` is safe
/// to read from two places, so there is nothing to coordinate.
pub async fn build_evaluator(external_url: &str) -> Result<AlertEvaluator> {
    let (client, namespace) = crate::kube_env::client_and_namespace().await?;
    info!(
        namespace = %namespace,
        configmap = %ALERTS_CONFIGMAP_NAME,
        external_url = %external_url,
        "Alerts subsystem enabled"
    );
    let store = AlertStore::new(client, namespace, ALERTS_CONFIGMAP_NAME.to_string());
    if let Err(e) = store.ensure_exists().await {
        warn!(error = %e, "Failed to pre-create alerts ConfigMap; will retry on first write");
    }
    let url = (!external_url.is_empty()).then(|| external_url.to_string());
    Ok(AlertEvaluator::with_external_url(store, url))
}
