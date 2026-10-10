//! The single place where resize outcomes fan out to the observation sinks:
//! metrics, Kubernetes Events, Alertmanager alerts, and Grafana annotations.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::Resizer;
use crate::awsx::Instance;
use crate::config::{
    ANNOTATE_ON_FAILURE, ANNOTATE_ON_SUCCESS, NOTIFY_ON_FAILURE, NOTIFY_ON_SUCCESS,
};
use crate::humanize::{go_duration, round_duration};
use crate::k8s::events::{TYPE_NORMAL, TYPE_WARNING, Target};
use crate::policy::Effective;
use crate::recstore::Entry;

/// Sends alerts about resize operations to an external sink such as
/// Alertmanager. `labels` carry per-alert identifying labels; `starts_at` is
/// the alert's start time.
#[async_trait]
pub trait AlertNotifier: Send + Sync {
    async fn notify(
        &self,
        severity: &str,
        alertname: &str,
        summary: &str,
        description: &str,
        labels: &BTreeMap<String, String>,
        starts_at: DateTime<Utc>,
    );
}

#[async_trait]
impl AlertNotifier for crate::alertmanager::Client {
    async fn notify(
        &self,
        severity: &str,
        alertname: &str,
        summary: &str,
        description: &str,
        labels: &BTreeMap<String, String>,
        starts_at: DateTime<Utc>,
    ) {
        Self::notify(
            self,
            severity,
            alertname,
            summary,
            description,
            labels,
            starts_at,
        )
        .await;
    }
}

/// Posts annotations about resize operations to a sink such as Grafana.
/// `end`, when set, makes it a region annotation spanning start..end.
#[async_trait]
pub trait Annotator: Send + Sync {
    async fn annotate(
        &self,
        text: &str,
        tags: &[String],
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    );
}

#[async_trait]
impl Annotator for crate::grafana::Client {
    async fn annotate(
        &self,
        text: &str,
        tags: &[String],
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) {
        Self::annotate(self, text, tags, start, end).await;
    }
}

/// Alert severities and names used for the alerts sent per resize operation.
const SEVERITY_WARNING: &str = "warning";
const SEVERITY_INFO: &str = "info";
const ALERT_RESIZE_FAILED: &str = "EBSRootVolumeAutoresizeFailed";
const ALERT_RESIZE_COMPLETED: &str = "EBSRootVolumeAutoresizeCompleted";

/// Event reasons. The Resize* reasons go to the addon's own Pod (standalone
/// EC2 instances have no Kubernetes object to attach to); the Volume*
/// reasons go to the target Node when the volume belongs to one.
const REASON_RESIZE_STARTED: &str = "ResizeStarted";
const REASON_RESIZE_COMPLETED: &str = "ResizeCompleted";
const REASON_RESIZE_FAILED: &str = "ResizeFailed";
const REASON_VOLUME_MODIFIED: &str = "VolumeModified";
const REASON_VOLUME_MODIFY_FAILED: &str = "VolumeModifyFailed";

/// One attempted volume modification: every dimension it would change and
/// the Node the volume belongs to, so each report sink states exactly what
/// was (or would have been) modified, instead of the size alone.
#[derive(Debug, Clone, Default)]
pub(super) struct ModSummary {
    /// Addresses the Node event; empty when the volume's node is unknown.
    pub node_name: String,
    pub node_uid: String,
    pub volume_id: String,
    pub size_from_gib: i32,
    pub size_to_gib: i32,
    /// `tp_to_mibps` zero means no throughput change was attempted.
    pub tp_from_mibps: i32,
    pub tp_to_mibps: i32,
    pub iops_from: i32,
    pub iops_to: i32,
    /// The combined request was rejected and the size change was applied
    /// alone.
    pub fallback: bool,
}

impl ModSummary {
    /// Renders every dimension the modification touches. Dimensions that stay
    /// untouched are omitted rather than reported unchanged.
    fn changes(&self) -> String {
        let mut s = format!(
            "size {} GiB to {} GiB",
            self.size_from_gib, self.size_to_gib
        );
        if self.tp_to_mibps > 0 && !self.fallback {
            let _ = write!(
                s,
                ", throughput {} to {} MiB/s",
                self.tp_from_mibps, self.tp_to_mibps
            );
            if self.iops_to != self.iops_from {
                let _ = write!(s, ", IOPS {} to {}", self.iops_from, self.iops_to);
            }
        }
        s
    }

    /// Names the throughput change that was attempted but not applied, or
    /// "" when there was none.
    fn fallback_note(&self) -> String {
        if !self.fallback {
            return String::new();
        }
        format!(
            " A piggybacked throughput increase ({} to {} MiB/s) was rejected by EC2 and was not applied; the size change proceeded alone.",
            self.tp_from_mibps, self.tp_to_mibps
        )
    }

    /// Describes the throughput change carried on the modification as a
    /// sentence for the alert description, or "" when the request was
    /// size-only.
    fn piggyback_note(&self) -> String {
        if self.tp_to_mibps == 0 || self.fallback {
            return self.fallback_note();
        }
        let mut s = format!(
            " Throughput was raised from {} to {} MiB/s",
            self.tp_from_mibps, self.tp_to_mibps
        );
        if self.iops_to != self.iops_from {
            let _ = write!(s, " (IOPS {} to {})", self.iops_from, self.iops_to);
        }
        s + " on the piggybacked recommendation."
    }
}

impl Resizer {
    /// Assembles the modification summary for one resize attempt.
    pub(super) fn new_mod_summary(
        &self,
        volume_id: &str,
        current: i32,
        target: i32,
        rec: &Entry,
        piggyback: bool,
    ) -> ModSummary {
        let mut m = ModSummary {
            volume_id: volume_id.into(),
            size_from_gib: current,
            size_to_gib: target,
            ..ModSummary::default()
        };
        if let Some((name, uid)) = self.recs.as_ref().and_then(|r| r.node_ref(volume_id)) {
            m.node_name = name;
            m.node_uid = uid;
        }
        if piggyback {
            m.tp_from_mibps = rec.current_mibps;
            m.tp_to_mibps = rec.throughput_mibps;
            m.iops_from = rec.current_iops;
            m.iops_to = rec.iops;
        }
        m
    }

    /// Publishes an Event against the volume's Node, when both the node and
    /// an emitter are known.
    fn emit_node(&self, m: &ModSummary, event_type: &str, reason: &str, message: String) {
        let Some(events) = &self.node_events else {
            return;
        };
        if m.node_name.is_empty() {
            return;
        }
        events.event(
            Target::node(&m.node_name, &m.node_uid),
            event_type,
            reason,
            message,
        );
    }

    /// Publishes a Kubernetes Event on the controller's own Pod.
    fn emit(&self, event_type: &str, reason: &str, message: String) {
        if let Some(e) = &self.events {
            e.emitter
                .event(e.target.clone(), event_type, reason, message);
        }
    }

    /// Announces a resize attempt before the first mutating AWS call.
    pub(super) fn report_started(&self, inst: &Instance, current: i32, target: i32, usage: i32) {
        self.emit(
            TYPE_NORMAL,
            REASON_RESIZE_STARTED,
            format!(
                "Resizing root filesystem on device {} of instance {} ({}) by growing volume {} from {current} GiB to {target} GiB (usage {usage}%)",
                inst.root_device_name, inst.name, inst.id, inst.root_volume_id
            ),
        );
    }

    /// Records one failed resize attempt across every sink. `stage` is the
    /// reconcile stage that failed (modify, wait, resize); `cause` names what
    /// went wrong.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn report_failure(
        &self,
        inst: &Instance,
        eff: &Effective,
        usage: i32,
        m: &ModSummary,
        stage: &str,
        cause: &str,
        start: DateTime<Utc>,
    ) {
        self.rec.observe_error(stage);
        self.rec.observe_resize(false, &eff.policy);
        let desc = failure_description(inst, usage, cause);
        self.emit(TYPE_WARNING, REASON_RESIZE_FAILED, desc.clone());
        // The Node event names every dimension the failed request attempted.
        self.emit_node(
            m,
            TYPE_WARNING,
            REASON_VOLUME_MODIFY_FAILED,
            format!(
                "Failed to modify EBS volume {} (attempted {}) at stage {stage:?}: {cause}",
                m.volume_id,
                m.changes()
            ),
        );
        self.notify(
            eff,
            SEVERITY_WARNING,
            ALERT_RESIZE_FAILED,
            "EBS root volume autoresize failed",
            &desc,
            &alert_labels(inst),
            start,
        )
        .await;
        // A failure is a point annotation at start.
        self.annotate(false, &desc, inst, start, None).await;
    }

    /// Records one completed resize across every sink.
    pub(super) async fn report_success(
        &self,
        inst: &Instance,
        eff: &Effective,
        usage: i32,
        after: i32,
        m: &ModSummary,
        start: DateTime<Utc>,
    ) {
        let target = m.size_to_gib;
        self.rec.observe_resize(true, &eff.policy);
        // Reflect the new size immediately instead of waiting for the next
        // pass.
        self.rec.observe_volume_size(
            &inst.id,
            &inst.root_device_name,
            &inst.root_volume_id,
            &inst.name,
            target,
        );
        let note = m.piggyback_note();
        let desc = format!(
            "Instance {} ({}) device {} was autoresized to {target} GiB. Root filesystem usage changed from {usage}% to {after}%.{note}",
            inst.id, inst.name, inst.root_device_name
        );
        let elapsed = (Utc::now() - start).to_std().unwrap_or_default();
        self.emit(
            TYPE_NORMAL,
            REASON_RESIZE_COMPLETED,
            format!(
                "Resized root filesystem on device {} of instance {} ({}) to {target} GiB in {}. Disk usage changed from {usage}% to {after}%{note}",
                inst.root_device_name,
                inst.name,
                inst.id,
                go_duration(round_duration(elapsed, std::time::Duration::from_secs(1)))
            ),
        );
        self.emit_node(
            m,
            TYPE_NORMAL,
            REASON_VOLUME_MODIFIED,
            format!(
                "Modified EBS volume {} in one modification slot: {}. Root filesystem usage changed from {usage}% to {after}%.{}",
                m.volume_id,
                m.changes(),
                m.fallback_note()
            ),
        );
        self.notify(
            eff,
            SEVERITY_INFO,
            ALERT_RESIZE_COMPLETED,
            "EBS root volume autoresize completed",
            &desc,
            &alert_labels(inst),
            start,
        )
        .await;
        // A completed resize is a region annotation spanning the time the
        // resize took.
        self.annotate(true, &desc, inst, start, Some(Utc::now()))
            .await;
    }

    /// Sends an alert when a notifier is configured, the instance's effective
    /// policy has alerting enabled, and the resize outcome matches the
    /// configured notify-on policy.
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        eff: &Effective,
        severity: &str,
        alertname: &str,
        summary: &str,
        description: &str,
        labels: &BTreeMap<String, String>,
        starts_at: DateTime<Utc>,
    ) {
        let Some(n) = &self.notifier else {
            return;
        };
        if !eff.alert_enabled {
            return;
        }
        match self.cfg.alertmanager_notify_on.as_str() {
            NOTIFY_ON_SUCCESS if severity != SEVERITY_INFO => return,
            NOTIFY_ON_FAILURE if severity != SEVERITY_WARNING => return,
            _ => {}
        }
        n.notify(severity, alertname, summary, description, labels, starts_at)
            .await;
    }

    /// Posts a Grafana annotation when an annotator is configured and the
    /// resize outcome matches the configured annotate-on policy.
    async fn annotate(
        &self,
        success: bool,
        text: &str,
        inst: &Instance,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) {
        let Some(a) = &self.annotator else {
            return;
        };
        match self.cfg.grafana_annotate_on.as_str() {
            ANNOTATE_ON_SUCCESS if !success => return,
            ANNOTATE_ON_FAILURE if success => return,
            _ => {}
        }
        a.annotate(text, &annotation_tags(inst, success), start, end)
            .await;
    }
}

/// The per-annotation tags identifying the instance and outcome, appended to
/// the configured base tags so dashboards can filter resize markers down to a
/// single disk or outcome.
fn annotation_tags(inst: &Instance, success: bool) -> Vec<String> {
    vec![
        format!("instance_id:{}", inst.id),
        format!("instance_name:{}", inst.name),
        format!("volume_id:{}", inst.root_volume_id),
        format!("device:{}", inst.root_device_name),
        format!("result:{}", if success { "success" } else { "failure" }),
    ]
}

/// The identifying labels attached to alerts for an instance.
fn alert_labels(inst: &Instance) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("instance_id".to_string(), inst.id.clone()),
        ("instance_name".to_string(), inst.name.clone()),
        ("volume_id".to_string(), inst.root_volume_id.clone()),
        ("device".to_string(), inst.root_device_name.clone()),
    ])
}

/// The alert description for a failed resize. The volume is not resized on
/// failure, so only the pre-resize usage is reported.
fn failure_description(inst: &Instance, usage: i32, reason: &str) -> String {
    format!(
        "Instance {} ({}) device {} failed to autoresize at {usage}% root filesystem usage. Cause: {reason}.",
        inst.id, inst.name, inst.root_device_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_text() {
        let mut m = ModSummary {
            volume_id: "vol-1".into(),
            size_from_gib: 100,
            size_to_gib: 110,
            ..ModSummary::default()
        };
        assert_eq!(m.changes(), "size 100 GiB to 110 GiB");
        assert_eq!(m.piggyback_note(), "");
        m.tp_from_mibps = 125;
        m.tp_to_mibps = 250;
        m.iops_from = 3000;
        m.iops_to = 3000;
        assert_eq!(
            m.changes(),
            "size 100 GiB to 110 GiB, throughput 125 to 250 MiB/s"
        );
        assert_eq!(
            m.piggyback_note(),
            " Throughput was raised from 125 to 250 MiB/s on the piggybacked recommendation."
        );
        m.iops_to = 4000;
        assert_eq!(
            m.changes(),
            "size 100 GiB to 110 GiB, throughput 125 to 250 MiB/s, IOPS 3000 to 4000"
        );
        m.fallback = true;
        assert_eq!(m.changes(), "size 100 GiB to 110 GiB");
        assert!(
            m.piggyback_note()
                .starts_with(" A piggybacked throughput increase (125 to 250 MiB/s) was rejected")
        );
    }

    #[test]
    fn labels_and_tags() {
        let inst = Instance {
            id: "i-1".into(),
            name: "web".into(),
            root_device_name: "/dev/xvda".into(),
            root_volume_id: "vol-1".into(),
            ..Instance::default()
        };
        assert_eq!(
            annotation_tags(&inst, true),
            vec![
                "instance_id:i-1",
                "instance_name:web",
                "volume_id:vol-1",
                "device:/dev/xvda",
                "result:success"
            ]
        );
        assert_eq!(annotation_tags(&inst, false)[4], "result:failure");
        let labels = alert_labels(&inst);
        assert_eq!(labels["device"], "/dev/xvda");
        assert_eq!(labels.len(), 4);
        assert_eq!(
            failure_description(&inst, 91, "boom"),
            "Instance i-1 (web) device /dev/xvda failed to autoresize at 91% root filesystem usage. Cause: boom."
        );
    }
}
