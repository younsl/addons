//! Reads `InstanceVolumeAutoscaler` resources and writes their status. The
//! resize pass lists them once per pass. A `kube::runtime::Controller`
//! validates each spec as soon as it changes, so a rejected spec shows up in
//! `kubectl get iva` without waiting for the next pass.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt as _;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::autoscaler::instance_volume::crd::{
    InstanceVolumeAutoscaler, InstanceVolumeAutoscalerStatus,
};
use crate::autoscaler::instance_volume::policy::compile;
use crate::autoscaler::{
    CONDITION_SCALING_ACTIVE, FIELD_MANAGER, STATUS_FALSE, STATUS_UNKNOWN, condition, set_condition,
};

/// The `ScalingActive` reason for a spec the controller rejects.
pub const REASON_INVALID_SPEC: &str = "InvalidSpec";
/// The `ScalingActive` reason for a valid spec the resize pass has not yet
/// evaluated.
pub const REASON_AWAITING_PASS: &str = "AwaitingReconcile";

#[derive(Debug, Error)]
pub enum AutoscalerError {
    #[error("kubernetes API: {0}")]
    Kube(#[from] kube::Error),
}

/// What the resizer needs from the autoscaler resources.
#[async_trait]
pub trait AutoscalerApi: Send + Sync {
    async fn list(&self) -> Result<Vec<InstanceVolumeAutoscaler>, String>;
    async fn patch_status(
        &self,
        name: &str,
        status: &InstanceVolumeAutoscalerStatus,
    ) -> Result<(), String>;
}

/// The kube-backed implementation.
pub struct KubeAutoscalers(Api<InstanceVolumeAutoscaler>);

impl KubeAutoscalers {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self(Api::all(client))
    }
}

#[async_trait]
impl AutoscalerApi for KubeAutoscalers {
    async fn list(&self) -> Result<Vec<InstanceVolumeAutoscaler>, String> {
        self.0
            .list(&kube::api::ListParams::default())
            .await
            .map(|l| l.items)
            .map_err(|e| e.to_string())
    }

    async fn patch_status(
        &self,
        name: &str,
        status: &InstanceVolumeAutoscalerStatus,
    ) -> Result<(), String> {
        let patch = serde_json::json!({ "status": status });
        self.0
            .patch_status(
                name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Merge(&patch),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// The status the validator wants for `iva`, or `None` when it has nothing
/// to change. A rejected spec turns `ScalingActive` false. A valid spec the
/// resize pass has not evaluated yet (new, or rejected before) turns it
/// unknown. Either way the one-line `kubectl get` fields are filled in right
/// away instead of after the next pass.
#[must_use]
pub fn validation_status(iva: &InstanceVolumeAutoscaler) -> Option<InstanceVolumeAutoscalerStatus> {
    let generation = iva.metadata.generation;
    let current = iva.status.clone().unwrap_or_default();
    let mut status = current.clone();
    let active = condition(&status.conditions, CONDITION_SCALING_ACTIVE);
    match compile(iva) {
        Err(err) => set_condition(
            &mut status.conditions,
            CONDITION_SCALING_ACTIVE,
            STATUS_FALSE,
            REASON_INVALID_SPEC,
            err,
            generation,
            Utc::now(),
        ),
        Ok(_) if active.is_none_or(|c| c.reason == REASON_INVALID_SPEC) => set_condition(
            &mut status.conditions,
            CONDITION_SCALING_ACTIVE,
            STATUS_UNKNOWN,
            REASON_AWAITING_PASS,
            "the spec is valid and waits for the next resize pass".into(),
            generation,
            Utc::now(),
        ),
        Ok(_) => {}
    }
    let conditions_changed = status.conditions != current.conditions;
    status.refresh_summary(&iva.spec);
    if status == current {
        return None;
    }
    if conditions_changed {
        status.observed_generation = generation;
    }
    Some(status)
}

struct Ctx {
    api: Arc<dyn AutoscalerApi>,
}

async fn reconcile(
    iva: Arc<InstanceVolumeAutoscaler>,
    ctx: Arc<Ctx>,
) -> Result<Action, AutoscalerError> {
    let name = iva.metadata.name.clone().unwrap_or_default();
    if let Some(status) = validation_status(&iva) {
        let reason = condition(&status.conditions, CONDITION_SCALING_ACTIVE)
            .map_or_default(|c| c.reason.clone());
        match ctx.api.patch_status(&name, &status).await {
            Ok(()) => {
                info!(autoscaler = %name, reason = %reason, "InstanceVolumeAutoscaler spec validated");
            }
            Err(err) => {
                warn!(autoscaler = %name, error = %err, "failed to write InstanceVolumeAutoscaler status");
            }
        }
    }
    Ok(Action::await_change())
}

#[allow(clippy::needless_pass_by_value)]
fn error_policy(_: Arc<InstanceVolumeAutoscaler>, err: &AutoscalerError, _: Arc<Ctx>) -> Action {
    warn!(error = %err, "InstanceVolumeAutoscaler validation failed");
    Action::requeue(Duration::from_mins(1))
}

/// Runs the spec validator until `shutdown` fires. Leader-only, like every
/// other status writer.
pub async fn run_validator(client: Client, shutdown: CancellationToken) {
    let ctx = Arc::new(Ctx {
        api: Arc::new(KubeAutoscalers::new(client.clone())),
    });
    Controller::new(
        Api::<InstanceVolumeAutoscaler>::all(client),
        watcher::Config::default(),
    )
    .graceful_shutdown_on(shutdown.cancelled_owned())
    .run(reconcile, error_policy, ctx)
    .for_each(|res| async move {
        if let Err(err) = res {
            debug!(error = %err, "InstanceVolumeAutoscaler controller event");
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::autoscaler::instance_volume::policy::tests::{by_id, iva};
    use crate::autoscaler::{CONDITION_ABLE_TO_SCALE, STATUS_TRUE};

    #[derive(Default)]
    pub(crate) struct Fake {
        patched: Mutex<Vec<(String, InstanceVolumeAutoscalerStatus)>>,
    }

    #[async_trait]
    impl AutoscalerApi for Fake {
        async fn list(&self) -> Result<Vec<InstanceVolumeAutoscaler>, String> {
            Ok(Vec::new())
        }
        async fn patch_status(
            &self,
            name: &str,
            status: &InstanceVolumeAutoscalerStatus,
        ) -> Result<(), String> {
            self.patched
                .lock()
                .unwrap()
                .push((name.into(), status.clone()));
            Ok(())
        }
    }

    #[test]
    fn validation_status_transitions() {
        let mut good = iva("ok", by_id("i-12345678"));
        let s = validation_status(&good).unwrap();
        assert_eq!(s.target, "i-12345678", "filled in before the first pass");
        assert_eq!(s.utilization, "<unknown>/80%");
        assert_eq!(s.reason, REASON_AWAITING_PASS);
        assert_eq!(s.current_volumes, 0);
        good.status = Some(s);
        assert!(validation_status(&good).is_none(), "nothing more to say");

        let mut bad = iva("bad", by_id("web-1"));
        let s = validation_status(&bad).unwrap();
        let c = condition(&s.conditions, CONDITION_SCALING_ACTIVE).unwrap();
        assert_eq!(c.status, STATUS_FALSE);
        assert_eq!(c.reason, REASON_INVALID_SPEC);
        assert!(
            c.message.contains("not an EC2 instance ID"),
            "{}",
            c.message
        );
        assert_eq!(s.observed_generation, Some(1));
        assert_eq!(s.reason, REASON_INVALID_SPEC);
        assert_eq!(s.target, "web-1", "readable even when rejected");

        bad.status = Some(s);
        assert!(validation_status(&bad).is_none(), "already reported");

        let mut fixed = iva("bad", by_id("i-12345678"));
        fixed.metadata.generation = Some(2);
        fixed.status = bad.status;
        set_condition(
            &mut fixed.status.as_mut().unwrap().conditions,
            CONDITION_ABLE_TO_SCALE,
            STATUS_TRUE,
            "X",
            String::new(),
            Some(1),
            Utc::now(),
        );
        let s = validation_status(&fixed).unwrap();
        let c = condition(&s.conditions, CONDITION_SCALING_ACTIVE).unwrap();
        assert_eq!(c.status, STATUS_UNKNOWN);
        assert_eq!(c.reason, REASON_AWAITING_PASS);
        assert_eq!(s.observed_generation, Some(2));
        assert!(condition(&s.conditions, CONDITION_ABLE_TO_SCALE).is_some());

        let mut edited = fixed.clone();
        edited.status = Some(s);
        edited.metadata.generation = Some(3);
        edited.spec.max_capacity =
            k8s_openapi::apimachinery::pkg::api::resource::Quantity("20Gi".into());
        assert!(
            validation_status(&edited).is_none(),
            "a valid edit that changes no shown field waits for the pass"
        );
        edited.spec.scale_target_ref =
            crate::autoscaler::instance_volume::policy::tests::by_tags(&[("Role", "db")]);
        let s = validation_status(&edited).unwrap();
        assert_eq!(s.target, "Role=db");
        assert_eq!(s.observed_generation, Some(2), "conditions did not change");
    }

    #[tokio::test]
    async fn reconcile_patches_only_when_needed() {
        let fake = Arc::new(Fake::default());
        let ctx = Arc::new(Ctx { api: fake.clone() });
        let mut ok = iva("ok", by_id("i-12345678"));
        ok.status = validation_status(&ok);
        reconcile(Arc::new(ok), ctx.clone()).await.unwrap();
        assert!(fake.patched.lock().unwrap().is_empty());
        reconcile(Arc::new(iva("bad", by_id("x"))), ctx.clone())
            .await
            .unwrap();
        assert_eq!(fake.patched.lock().unwrap()[0].0, "bad");
        let action = error_policy(
            Arc::new(iva("bad", by_id("x"))),
            &AutoscalerError::Kube(kube::Error::LinesCodecMaxLineLengthExceeded),
            ctx,
        );
        assert_eq!(action, Action::requeue(Duration::from_mins(1)));
    }
}
