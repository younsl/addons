//! One-shot migration of alert rules out of the legacy ConfigMap.
//!
//! Before the CRD, every rule was a JSON value under its own key in a single
//! `trivy-collector-alerts` ConfigMap. This reads that object and applies one
//! `AlertRule` custom resource per key.
//!
//! Three properties make it safe to run unattended on every startup:
//!
//! - it is idempotent, because a key whose rule already exists is skipped;
//! - it never deletes the ConfigMap, so the ConfigMap stays the rollback until
//!   an operator removes it;
//! - it stamps the ConfigMap when it finishes, so a restart is a no-op rather
//!   than a re-import that would resurrect a rule the operator has since
//!   deleted.
//!
//! That last point is the one that matters. Without the stamp, deleting a
//! migrated rule in the UI would bring it back on the next pod restart.

use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Client,
    api::{Api, Patch, PatchParams},
};
use tracing::{info, warn};

use super::store::AlertStore;
use super::types::AlertRule;

/// Name of the ConfigMap rules used to live in.
pub const LEGACY_CONFIGMAP_NAME: &str = "trivy-collector-alerts";

/// Set on the legacy ConfigMap once its rules have been imported.
pub const MIGRATED_AT_ANNOTATION: &str = "trivy-collector.security.io/migrated-at";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationSummary {
    pub imported: usize,
    /// Rules whose name already existed as a custom resource.
    pub skipped_existing: usize,
    /// Keys that did not deserialize, or that the store rejected.
    pub failed: usize,
}

impl MigrationSummary {
    fn touched_nothing(&self) -> bool {
        self.imported == 0 && self.skipped_existing == 0 && self.failed == 0
    }
}

/// Import the legacy ConfigMap's rules, if there is anything left to import.
///
/// Returns `None` when there is no work: no ConfigMap, an empty one, or one
/// already stamped as migrated. Errors are logged and swallowed by the caller
/// — a failed import must not stop the process from starting, because the
/// alerts subsystem is still usable for rules authored against the CRD.
pub async fn run(
    client: Client,
    namespace: &str,
    store: &AlertStore,
) -> Result<Option<MigrationSummary>, kube::Error> {
    let api: Api<ConfigMap> = Api::namespaced(client, namespace);
    let Some(cm) = api.get_opt(LEGACY_CONFIGMAP_NAME).await? else {
        return Ok(None);
    };
    if let Some(at) = cm
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(MIGRATED_AT_ANNOTATION))
    {
        info!(
            configmap = LEGACY_CONFIGMAP_NAME,
            migrated_at = %at,
            "Legacy alert ConfigMap already migrated; it can be deleted"
        );
        return Ok(None);
    }
    let data = cm.data.unwrap_or_default();
    if data.is_empty() {
        return Ok(None);
    }

    let existing: Vec<String> = match store.list().await {
        Ok(rules) => rules.into_iter().map(|r| r.name).collect(),
        Err(e) => {
            warn!(error = %e, "Cannot read existing alert rules; skipping ConfigMap migration");
            return Ok(None);
        }
    };

    let mut summary = MigrationSummary::default();
    for (key, value) in &data {
        let rule: AlertRule = match serde_json::from_str(value) {
            Ok(r) => r,
            Err(e) => {
                warn!(rule = %key, error = %e, "Skipping malformed legacy alert rule");
                summary.failed += 1;
                continue;
            }
        };
        // The ConfigMap key and the rule's own `name` were written together,
        // but the key is what addressed the rule, so it wins.
        let rule = AlertRule {
            name: key.clone(),
            ..rule
        };
        if existing.contains(&rule.name) {
            summary.skipped_existing += 1;
            continue;
        }
        match store.upsert(&rule).await {
            Ok(_) => {
                info!(rule = %rule.name, "Imported alert rule from legacy ConfigMap");
                summary.imported += 1;
            }
            Err(e) => {
                warn!(rule = %rule.name, error = %e, "Failed to import legacy alert rule");
                summary.failed += 1;
            }
        }
    }

    // Stamp only when nothing failed. A partial import left unstamped is
    // retried on the next start, which is the recoverable outcome; stamping it
    // would strand whichever rules did not make it.
    if summary.failed == 0 {
        stamp(&api).await?;
        info!(
            imported = summary.imported,
            skipped_existing = summary.skipped_existing,
            configmap = LEGACY_CONFIGMAP_NAME,
            "Alert rule migration complete; the legacy ConfigMap is no longer read and can be deleted"
        );
    } else {
        warn!(
            imported = summary.imported,
            failed = summary.failed,
            "Alert rule migration incomplete; will retry on next start"
        );
    }

    Ok((!summary.touched_nothing()).then_some(summary))
}

async fn stamp(api: &Api<ConfigMap>) -> Result<(), kube::Error> {
    let patch = serde_json::json!({
        "metadata": {
            "annotations": {
                MIGRATED_AT_ANNOTATION: chrono::Utc::now().to_rfc3339(),
            }
        }
    });
    api.patch(
        LEGACY_CONFIGMAP_NAME,
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_that_touched_nothing_is_reported_as_no_work() {
        assert!(MigrationSummary::default().touched_nothing());
        assert!(
            !MigrationSummary {
                imported: 1,
                ..Default::default()
            }
            .touched_nothing()
        );
        assert!(
            !MigrationSummary {
                failed: 1,
                ..Default::default()
            }
            .touched_nothing()
        );
        assert!(
            !MigrationSummary {
                skipped_existing: 1,
                ..Default::default()
            }
            .touched_nothing()
        );
    }

    /// The legacy names are the contract with every already-deployed release,
    /// so a rename here would silently orphan an operator's rules.
    #[test]
    fn the_legacy_object_and_stamp_names_are_pinned() {
        assert_eq!(LEGACY_CONFIGMAP_NAME, "trivy-collector-alerts");
        assert_eq!(
            MIGRATED_AT_ANNOTATION,
            "trivy-collector.security.io/migrated-at"
        );
    }

    /// A legacy value's `name` field and its ConfigMap key could disagree.
    /// The key addressed the rule, so it has to win, otherwise migrating
    /// would rename a rule out from under whoever references it.
    #[test]
    fn the_configmap_key_wins_over_the_embedded_name() {
        let json = serde_json::json!({
            "name": "stale-embedded-name",
            "description": "",
            "enabled": true,
            "matchers": {"package_name": "axios"},
            "labels": {},
            "annotations": {},
            "receivers": [{"name": "sec", "slack": {
                "webhook_url": "https://hooks.slack.com/services/T0/B0/x"
            }}],
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "alice",
        })
        .to_string();

        let parsed: AlertRule = serde_json::from_str(&json).unwrap();
        let renamed = AlertRule {
            name: "addressed-by-this-key".to_string(),
            ..parsed
        };
        assert_eq!(renamed.name, "addressed-by-this-key");
        assert_eq!(renamed.created_by, "alice");
        assert_eq!(renamed.matchers.package_name.as_deref(), Some("axios"));
    }
}
