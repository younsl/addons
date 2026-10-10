//! Protective cordon: while an in-cluster Node's root filesystem is at or
//! above its policy's usage threshold, the Node is cordoned so the scheduler
//! stops placing new Pods on a filling disk, and it is uncordoned once usage
//! is back under. It brackets the resize rather than replacing it: the
//! cordon stays on through a cooldown or a max-size skip, which are exactly
//! the cases where the volume cannot grow in time. It is always on for a
//! measured instance that maps to a Node, and the `ProtectiveCordon` Node
//! condition mirrors whether the addon holds the cordon.

use std::collections::HashMap;

use chrono::Utc;
use tracing::{debug, info, warn};

use super::Resizer;
use crate::awsx::Instance;
use crate::k8s::cordon::{Condition, CordonNode};
use crate::k8s::events::{TYPE_NORMAL, TYPE_WARNING, Target};
use crate::policy::Effective;

pub const ACTION_CORDON: &str = "cordon";
pub const ACTION_UNCORDON: &str = "uncordon";
pub const RESULT_SUCCESS: &str = "success";
pub const RESULT_FAILURE: &str = "failure";

const REASON_APPLIED: &str = "ProtectiveCordonApplied";
const REASON_RELEASED: &str = "ProtectiveCordonReleased";
const REASON_MARK_REMOVED: &str = "ProtectiveCordonMarkRemoved";

impl Resizer {
    /// Lists the cluster's EC2-backed Nodes once per pass, keyed by instance
    /// ID. A failed list only costs this pass its cordon decisions: the
    /// resize itself never depends on a Node.
    pub(super) async fn protective_cordon_nodes(&self) -> HashMap<String, CordonNode> {
        let Some(api) = &self.cordon else {
            return HashMap::new();
        };
        match api.list().await {
            Ok(nodes) => nodes,
            Err(err) => {
                self.rec.observe_error("protective_cordon");
                warn!(error = %err, "listing Nodes failed; protective cordon decisions are skipped this pass");
                HashMap::new()
            }
        }
    }

    /// Cordons the instance's Node, then brings its condition in line.
    pub(super) async fn apply_protective_cordon(
        &self,
        inst: &Instance,
        node: &mut Option<CordonNode>,
        usage: i32,
        eff: &Effective,
    ) {
        self.cordon_node(inst, node, usage, eff).await;
        self.sync_protective_condition(inst, node, usage, eff).await;
    }

    /// Lifts the addon's own cordon, then brings the Node's condition in line.
    pub(super) async fn release_protective_cordon(
        &self,
        inst: &Instance,
        node: &mut Option<CordonNode>,
        usage: i32,
        eff: &Effective,
    ) {
        self.uncordon_node(inst, node, usage, eff).await;
        self.sync_protective_condition(inst, node, usage, eff).await;
    }

    /// A Node that is already unschedulable without the addon's mark belongs
    /// to someone else's cordon and is left alone, so it is never later
    /// uncordoned by the addon either.
    async fn cordon_node(
        &self,
        inst: &Instance,
        node: &mut Option<CordonNode>,
        usage: i32,
        eff: &Effective,
    ) {
        let (Some(api), Some(n)) = (&self.cordon, node.as_mut()) else {
            return;
        };
        let (instance, policy, node_name) =
            (inst.id.as_str(), eff.policy.as_str(), n.name.as_str());
        let threshold = eff.usage_threshold_percent;
        if n.protective {
            debug!(
                instance,
                policy,
                node = node_name,
                "protective cordon already in place"
            );
            return;
        }
        if n.unschedulable {
            info!(
                instance,
                policy,
                node = node_name,
                usage_percent = usage,
                "Node is already cordoned by someone else; protective cordon not applied"
            );
            return;
        }
        if self.cfg.dry_run {
            info!(
                instance,
                policy,
                node = node_name,
                usage_percent = usage,
                threshold_percent = threshold,
                "dry-run: would apply protective cordon"
            );
            return;
        }
        if let Err(err) = api.cordon(node_name, Utc::now()).await {
            self.rec
                .observe_protective_cordon(ACTION_CORDON, RESULT_FAILURE);
            warn!(instance, policy, node = node_name, error = %err, "protective cordon failed");
            return;
        }
        self.rec
            .observe_protective_cordon(ACTION_CORDON, RESULT_SUCCESS);
        info!(
            instance,
            policy,
            node = node_name,
            usage_percent = usage,
            threshold_percent = threshold,
            "applied protective cordon: Node is unschedulable until root usage falls back under the threshold"
        );
        self.emit_cordon(
            n,
            TYPE_WARNING,
            REASON_APPLIED,
            format!(
                "Protective cordon applied by {} because root filesystem usage {usage}% reached the {threshold}% threshold.",
                crate::k8s::events::COMPONENT
            ),
        );
        n.unschedulable = true;
        n.protective = true;
    }

    /// Lifts the addon's own cordon once usage is back under the threshold.
    /// A mark left on a Node someone else already uncordoned is just dropped.
    async fn uncordon_node(
        &self,
        inst: &Instance,
        node: &mut Option<CordonNode>,
        usage: i32,
        eff: &Effective,
    ) {
        let (Some(api), Some(n)) = (&self.cordon, node.as_mut()) else {
            return;
        };
        if !n.protective {
            return;
        }
        let (instance, policy, node_name) =
            (inst.id.as_str(), eff.policy.as_str(), n.name.as_str());
        let threshold = eff.usage_threshold_percent;
        if self.cfg.dry_run {
            info!(
                instance,
                policy,
                node = node_name,
                usage_percent = usage,
                threshold_percent = threshold,
                "dry-run: would release protective cordon"
            );
            return;
        }
        if !n.unschedulable {
            if let Err(err) = api.forget(node_name).await {
                warn!(instance, policy, node = node_name, error = %err, "dropping protective cordon mark failed");
                return;
            }
            info!(
                instance,
                policy,
                node = node_name,
                "Node was already uncordoned by someone else; dropped the protective cordon mark"
            );
            n.protective = false;
            return;
        }
        if let Err(err) = api.uncordon(node_name).await {
            self.rec
                .observe_protective_cordon(ACTION_UNCORDON, RESULT_FAILURE);
            warn!(instance, policy, node = node_name, error = %err, "releasing protective cordon failed");
            return;
        }
        self.rec
            .observe_protective_cordon(ACTION_UNCORDON, RESULT_SUCCESS);
        info!(
            instance,
            policy,
            node = node_name,
            usage_percent = usage,
            threshold_percent = threshold,
            "released protective cordon"
        );
        self.emit_cordon(
            n,
            TYPE_NORMAL,
            REASON_RELEASED,
            format!(
                "Protective cordon released by {} because root filesystem usage {usage}% is back under the {threshold}% threshold.",
                crate::k8s::events::COMPONENT
            ),
        );
        n.unschedulable = false;
        n.protective = false;
    }

    /// Writes the `ProtectiveCordon` condition when it disagrees with the
    /// mark: `True` while the addon holds the cordon, `False` once it no
    /// longer does. `False` says who ended it: `ProtectiveCordonReleased` when
    /// the Node is schedulable again with usage back under the threshold,
    /// `ProtectiveCordonMarkRemoved` when someone else removed the mark. A Node with no mark that never carried the condition is
    /// not written, so most Nodes never get it. A failed write is retried on
    /// the next pass, which also backfills cordons applied before the
    /// condition existed.
    async fn sync_protective_condition(
        &self,
        inst: &Instance,
        node: &mut Option<CordonNode>,
        usage: i32,
        eff: &Effective,
    ) {
        let (Some(api), Some(n)) = (&self.cordon, node.as_mut()) else {
            return;
        };
        let want = n.protective;
        if n.condition == Some(want) || (!want && n.condition.is_none()) || self.cfg.dry_run {
            return;
        }
        let component = crate::k8s::events::COMPONENT;
        let (reason, message) = if want {
            (
                REASON_APPLIED,
                format!("{component} has cordoned the node for high root filesystem usage"),
            )
        } else if usage < eff.usage_threshold_percent && !n.unschedulable {
            (
                REASON_RELEASED,
                format!(
                    "{component} has no cordon as root filesystem usage is back under the threshold"
                ),
            )
        } else {
            (
                REASON_MARK_REMOVED,
                format!("{component} has no cordon as its mark was removed by someone else"),
            )
        };
        let condition = Condition {
            status: want,
            reason,
            message,
            at: Utc::now(),
        };
        let (instance, policy, node_name) =
            (inst.id.as_str(), eff.policy.as_str(), n.name.as_str());
        if let Err(err) = api.set_condition(node_name, &condition).await {
            self.rec.observe_error("protective_cordon");
            warn!(instance, policy, node = node_name, error = %err, "writing the ProtectiveCordon condition failed");
            return;
        }
        debug!(
            instance,
            policy,
            node = node_name,
            status = want,
            "wrote the ProtectiveCordon condition"
        );
        n.condition = Some(want);
    }

    fn emit_cordon(&self, n: &CordonNode, event_type: &str, reason: &str, message: String) {
        if let Some(events) = &self.node_events {
            events.event(Target::node(&n.name, &n.uid), event_type, reason, message);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::awsx::VolumeModification;
    use crate::config::Config;
    use crate::resizer::tests::{
        FakeCordon, FakeEc2, FakeSsm, Harness, config, harness_with_cordon, instance,
    };

    /// A Node in a consistent state: the condition is `True` exactly when the
    /// addon holds the cordon.
    fn node(unschedulable: bool, protective: bool) -> CordonNode {
        CordonNode {
            name: "n1".into(),
            uid: "u1".into(),
            unschedulable,
            protective,
            condition: protective.then_some(true),
        }
    }

    fn cordon_with(n: Option<CordonNode>) -> FakeCordon {
        FakeCordon {
            nodes: Mutex::new(n.into_iter().map(|n| ("i-1".to_string(), n)).collect()),
            ..FakeCordon::default()
        }
    }

    fn ec2() -> FakeEc2 {
        let ec2 = FakeEc2::default();
        *ec2.instances.lock().unwrap() = vec![instance("i-1", 100)];
        ec2
    }

    fn ssm(usages: &[&str]) -> FakeSsm {
        FakeSsm {
            measurements: Mutex::new(usages.iter().map(|u| Ok((*u).to_string())).collect()),
            ..FakeSsm::default()
        }
    }

    async fn run(cfg: Config, ec2: FakeEc2, usages: &[&str], cordon: FakeCordon) -> Harness {
        let h = harness_with_cordon(cfg, ec2, ssm(usages), None, cordon);
        h.resizer.reconcile().await.unwrap();
        h.flush().await;
        h
    }

    fn calls(h: &Harness) -> Vec<(String, String)> {
        h.cordon.calls.lock().unwrap().clone()
    }

    fn reasons(h: &Harness) -> Vec<(bool, String)> {
        h.cordon.conditions.lock().unwrap().clone()
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
            .collect()
    }

    #[tokio::test]
    async fn cordons_over_threshold_and_releases_after_the_resize() {
        let h = run(
            config(),
            ec2(),
            &["85", "60"],
            cordon_with(Some(node(false, false))),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[
                ("cordon", "n1"),
                ("condition=true", "n1"),
                ("uncordon", "n1"),
                ("condition=false", "n1"),
            ])
        );
        assert_eq!(
            h.rec.cordons.lock().unwrap().clone(),
            pairs(&[("cordon", "success"), ("uncordon", "success")])
        );
        assert_eq!(
            reasons(&h),
            vec![
                (true, REASON_APPLIED.to_string()),
                (false, REASON_RELEASED.to_string())
            ]
        );
        let events = h.node_events.summary();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].0.as_str(), events[0].2.as_str()), ("Node", "n1"));
        assert_eq!(
            (events[0].3.as_str(), events[0].4.as_str()),
            ("Warning", REASON_APPLIED)
        );
        assert!(
            events[0].5.contains("usage 85% reached the 80% threshold"),
            "{}",
            events[0].5
        );
        assert_eq!(
            (events[1].3.as_str(), events[1].4.as_str()),
            ("Normal", REASON_RELEASED)
        );
        assert!(
            events[1]
                .5
                .contains("usage 60% is back under the 80% threshold"),
            "{}",
            events[1].5
        );
        assert_eq!(
            h.ec2.modify_calls.lock().unwrap().len(),
            1,
            "the resize still runs"
        );
    }

    #[tokio::test]
    async fn stays_cordoned_while_the_volume_cannot_grow() {
        let ec2 = ec2();
        *ec2.last_modification.lock().unwrap() = Some(VolumeModification {
            state: "completed".into(),
            start_time: Some(Utc::now()),
            target_gib: 100,
        });
        let h = run(
            config(),
            ec2,
            &["90"],
            cordon_with(Some(node(false, false))),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[("cordon", "n1"), ("condition=true", "n1")])
        );
        assert_eq!(h.ec2.modify_calls.lock().unwrap().len(), 0);

        let h = run(
            config(),
            self::ec2(),
            &["90", "85"],
            cordon_with(Some(node(false, false))),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[("cordon", "n1"), ("condition=true", "n1")]),
            "still above after the resize"
        );
    }

    #[tokio::test]
    async fn releases_only_its_own_cordon() {
        let h = run(
            config(),
            ec2(),
            &["50"],
            cordon_with(Some(node(true, true))),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[("uncordon", "n1"), ("condition=false", "n1")])
        );

        let h = run(
            config(),
            ec2(),
            &["50"],
            cordon_with(Some(node(false, true))),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[("forget", "n1"), ("condition=false", "n1")]),
            "uncordoned by someone else"
        );
        assert_eq!(reasons(&h), vec![(false, REASON_RELEASED.to_string())]);
        assert_eq!(h.rec.cordons.lock().unwrap().len(), 0);

        let h = run(
            config(),
            ec2(),
            &["50"],
            cordon_with(Some(node(true, false))),
        )
        .await;
        assert!(
            calls(&h).is_empty(),
            "someone else's cordon below threshold"
        );
        let h = run(
            config(),
            ec2(),
            &["90", "50"],
            cordon_with(Some(node(true, false))),
        )
        .await;
        assert!(
            calls(&h).is_empty(),
            "someone else's cordon above threshold"
        );
        assert_eq!(h.node_events.summary().len(), 0);
    }

    #[tokio::test]
    async fn condition_follows_the_mark() {
        let unmirrored = CordonNode {
            condition: None,
            ..node(true, true)
        };
        let h = run(
            config(),
            ec2(),
            &["90", "85"],
            cordon_with(Some(unmirrored)),
        )
        .await;
        assert_eq!(
            calls(&h),
            pairs(&[("condition=true", "n1")]),
            "a cordon from before the condition existed is backfilled"
        );

        let stale = CordonNode {
            condition: Some(true),
            ..node(true, false)
        };
        let h = run(config(), ec2(), &["90", "50"], cordon_with(Some(stale))).await;
        assert_eq!(
            calls(&h),
            pairs(&[("condition=false", "n1")]),
            "the mark was removed by hand"
        );
        assert_eq!(reasons(&h), vec![(false, REASON_MARK_REMOVED.to_string())]);

        let held_by_operator = CordonNode {
            condition: Some(true),
            ..node(true, false)
        };
        let h = run(
            config(),
            ec2(),
            &["50"],
            cordon_with(Some(held_by_operator)),
        )
        .await;
        assert_eq!(
            reasons(&h),
            vec![(false, REASON_MARK_REMOVED.to_string())],
            "usage is back under but the Node is still cordoned by someone else"
        );

        let removed_then_uncordoned = CordonNode {
            condition: Some(true),
            ..node(false, false)
        };
        let h = run(
            config(),
            ec2(),
            &["50"],
            cordon_with(Some(removed_then_uncordoned)),
        )
        .await;
        assert_eq!(reasons(&h), vec![(false, REASON_RELEASED.to_string())]);

        let released = CordonNode {
            condition: Some(false),
            ..node(false, false)
        };
        let h = run(config(), ec2(), &["50"], cordon_with(Some(released))).await;
        assert!(calls(&h).is_empty(), "already False");
    }

    #[tokio::test]
    async fn no_op_cases() {
        let h = run(
            config(),
            ec2(),
            &["90", "85"],
            cordon_with(Some(node(true, true))),
        )
        .await;
        assert_eq!(calls(&h), pairs(&[]), "already cordoned by the addon");

        let h = run(config(), ec2(), &["90", "50"], cordon_with(None)).await;
        assert_eq!(calls(&h), pairs(&[]), "standalone EC2 has no Node");

        let mut dry = config();
        dry.dry_run = true;
        let h = run(
            dry.clone(),
            ec2(),
            &["90"],
            cordon_with(Some(node(false, false))),
        )
        .await;
        assert_eq!(calls(&h), pairs(&[]));
        let h = run(dry, ec2(), &["50"], cordon_with(Some(node(true, true)))).await;
        assert_eq!(calls(&h), pairs(&[]));

        let mut paused = config();
        paused.paused = true;
        let h = run(
            paused,
            ec2(),
            &["90"],
            cordon_with(Some(node(false, false))),
        )
        .await;
        assert_eq!(calls(&h), pairs(&[]), "a paused instance is never measured");
    }

    #[tokio::test]
    async fn failures_never_block_the_resize() {
        let cordon = FakeCordon {
            list_error: Some("forbidden".into()),
            ..cordon_with(Some(node(false, false)))
        };
        let h = run(config(), ec2(), &["90", "50"], cordon).await;
        assert_eq!(calls(&h), pairs(&[]));
        assert_eq!(
            h.rec.errors.lock().unwrap().clone(),
            vec!["protective_cordon"]
        );
        assert_eq!(h.ec2.modify_calls.lock().unwrap().len(), 1);

        let cordon = FakeCordon {
            fail: true,
            ..cordon_with(Some(node(false, false)))
        };
        let h = run(config(), ec2(), &["90", "50"], cordon).await;
        assert_eq!(
            calls(&h),
            pairs(&[("cordon", "n1")]),
            "a failed cordon is not released"
        );
        assert_eq!(
            h.rec.cordons.lock().unwrap().clone(),
            pairs(&[("cordon", "failure")])
        );
        assert_eq!(h.ec2.modify_calls.lock().unwrap().len(), 1);

        let cordon = FakeCordon {
            fail: true,
            ..cordon_with(Some(node(true, true)))
        };
        let h = run(config(), ec2(), &["50"], cordon).await;
        assert_eq!(
            h.rec.cordons.lock().unwrap().clone(),
            pairs(&[("uncordon", "failure")])
        );
        assert_eq!(
            calls(&h),
            pairs(&[("uncordon", "n1")]),
            "the condition stays True"
        );
        let cordon = FakeCordon {
            fail: true,
            ..cordon_with(Some(node(false, true)))
        };
        let h = run(config(), ec2(), &["50"], cordon).await;
        assert_eq!(calls(&h), pairs(&[("forget", "n1")]));
        assert_eq!(h.node_events.summary().len(), 0);

        let cordon = FakeCordon {
            fail_condition: true,
            ..cordon_with(Some(node(false, false)))
        };
        let h = run(config(), ec2(), &["90", "50"], cordon).await;
        assert_eq!(
            calls(&h),
            pairs(&[
                ("cordon", "n1"),
                ("condition=true", "n1"),
                ("uncordon", "n1")
            ]),
            "a failed condition write never blocks the cordon"
        );
        assert_eq!(
            h.rec.errors.lock().unwrap().clone(),
            vec!["protective_cordon"]
        );
        assert_eq!(h.ec2.modify_calls.lock().unwrap().len(), 1);
    }
}
