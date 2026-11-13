//! Decides which node gets which revision, the part of a `Deployment`
//! controller that scales `ReplicaSet` objects against `maxUnavailable`.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use node_configuration_common::crd::binding::NodeConfigurationBinding;
use node_configuration_common::crd::{NodeConfigurationSpec, StrategyType};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Nodes the target selects.
    pub desired: i32,
    /// Nodes assigned the current revision.
    pub updated: i32,
    /// Nodes whose last run applied their assigned revision cleanly.
    pub ready: i32,
    /// Ready nodes that have stayed so for `minReadySeconds`.
    pub available: i32,
}

/// A binding to create for a node that has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Create {
    pub node: String,
    /// Index of the binding whose revision the node gets while the rollout is
    /// paused. `None` assigns the current revision.
    pub held: Option<usize>,
}

/// What to change, as indices into the bindings the plan was made from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub create: Vec<Create>,
    /// Bindings to move to the current revision.
    pub update: Vec<usize>,
    /// Bindings of nodes the target no longer selects.
    pub delete: Vec<usize>,
    /// The counts once the plan is carried out.
    pub counts: Counts,
}

fn count(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// The `maxUnavailable` budget in nodes. A percentage rounds up, and the
/// budget is never below one or a rollout could not start.
pub fn max_unavailable(spec: &NodeConfigurationSpec, desired: usize) -> usize {
    let value = spec
        .strategy
        .rolling_update
        .as_ref()
        .and_then(|r| r.max_unavailable.as_ref());
    let budget = match value {
        None => 1,
        Some(IntOrString::Int(n)) => usize::try_from(*n).unwrap_or(1),
        Some(IntOrString::String(percent)) => percent
            .strip_suffix('%')
            .and_then(|p| p.parse::<usize>().ok())
            .map_or(1, |p| (desired * p).div_ceil(100)),
    };
    budget.max(1)
}

fn ready(binding: &NodeConfigurationBinding) -> bool {
    binding.status.as_ref().is_some_and(|s| {
        s.ready && s.observed_revision.as_deref() == Some(binding.spec.revision.as_str())
    })
}

/// Ready, and for at least `min_ready_seconds`.
pub fn available(
    binding: &NodeConfigurationBinding,
    min_ready_seconds: u32,
    now: DateTime<Utc>,
) -> bool {
    if !ready(binding) {
        return false;
    }
    if min_ready_seconds == 0 {
        return true;
    }
    binding
        .status
        .as_ref()
        .and_then(|s| s.last_transition_time.as_deref())
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|since| {
            (now - since.with_timezone(&Utc)).num_seconds() >= i64::from(min_ready_seconds)
        })
}

pub fn plan(
    spec: &NodeConfigurationSpec,
    revision: &str,
    nodes: &BTreeSet<String>,
    bindings: &[NodeConfigurationBinding],
    now: DateTime<Utc>,
) -> Plan {
    let mut plan = Plan::default();
    let mut live: Vec<usize> = Vec::new();
    for (i, binding) in bindings.iter().enumerate() {
        if nodes.contains(&binding.spec.node_name) {
            live.push(i);
        } else {
            plan.delete.push(i);
        }
    }
    live.sort_by(|a, b| {
        bindings[*a]
            .spec
            .node_name
            .cmp(&bindings[*b].spec.node_name)
    });

    let is_current = |i: usize| bindings[i].spec.revision == revision;
    let is_available = |i: usize| available(&bindings[i], spec.min_ready_seconds, now);

    let held = spec
        .paused
        .then(|| live.iter().copied().find(|i| !is_current(*i)))
        .flatten();
    let bound: BTreeSet<&str> = live
        .iter()
        .map(|i| bindings[*i].spec.node_name.as_str())
        .collect();
    plan.create = nodes
        .iter()
        .filter(|node| !bound.contains(node.as_str()))
        .map(|node| Create {
            node: node.clone(),
            held,
        })
        .collect();

    if !spec.paused && spec.strategy.type_ == StrategyType::RollingUpdate {
        let budget = max_unavailable(spec, nodes.len());
        let mut unavailable =
            live.iter().filter(|i| !is_available(**i)).count() + plan.create.len();
        let mut old: Vec<usize> = live.iter().copied().filter(|i| !is_current(*i)).collect();
        // A node that is already unavailable costs no budget, so it goes first.
        old.sort_by_key(|i| is_available(*i));
        for i in old {
            if !is_available(i) {
                plan.update.push(i);
            } else if unavailable < budget {
                plan.update.push(i);
                unavailable += 1;
            }
        }
    }

    let untouched: Vec<usize> = live
        .iter()
        .copied()
        .filter(|i| !plan.update.contains(i))
        .collect();
    let created_current = plan.create.iter().filter(|c| c.held.is_none()).count();
    plan.counts = Counts {
        desired: count(nodes.len()),
        updated: count(
            untouched.iter().filter(|i| is_current(**i)).count()
                + plan.update.len()
                + created_current,
        ),
        ready: count(untouched.iter().filter(|i| ready(&bindings[**i])).count()),
        available: count(untouched.iter().filter(|i| is_available(**i)).count()),
    };
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use node_configuration_common::crd::binding::{
        NodeConfigurationBindingSpec, NodeConfigurationBindingStatus,
    };
    use node_configuration_common::crd::testing::spec;
    use node_configuration_common::crd::{RollingUpdate, Rules};

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).unwrap()
    }

    fn binding(
        node: &str,
        revision: &str,
        applied: Option<(&str, bool)>,
    ) -> NodeConfigurationBinding {
        let mut binding = NodeConfigurationBinding::new(
            &format!("mng.{node}"),
            NodeConfigurationBindingSpec {
                configuration: "mng".to_string(),
                node_name: node.to_string(),
                revision: revision.to_string(),
                group: None,
                priority: 0,
                dry_run: false,
                interval_seconds: 300,
                rules: Rules::default(),
            },
        );
        binding.status = applied.map(|(observed, ready)| NodeConfigurationBindingStatus {
            observed_revision: Some(observed.to_string()),
            ready,
            failed_rules: i32::from(!ready),
            message: None,
            rules: Vec::new(),
            last_transition_time: Some("2027-01-15T07:59:00Z".to_string()),
            ..NodeConfigurationBindingStatus::default()
        });
        binding
    }

    fn nodes(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn with_budget(value: IntOrString) -> NodeConfigurationSpec {
        let mut s = spec(Rules::default());
        s.strategy.rolling_update = Some(RollingUpdate {
            max_unavailable: Some(value),
            cordon: false,
        });
        s
    }

    #[test]
    fn budget_is_a_count_or_a_rounded_up_percentage() {
        assert_eq!(max_unavailable(&spec(Rules::default()), 10), 1);
        assert_eq!(max_unavailable(&with_budget(IntOrString::Int(3)), 10), 3);
        assert_eq!(
            max_unavailable(&with_budget(IntOrString::String("25%".into())), 10),
            3
        );
        assert_eq!(
            max_unavailable(&with_budget(IntOrString::String("10%".into())), 3),
            1
        );
        assert_eq!(
            max_unavailable(&with_budget(IntOrString::String("100%".into())), 4),
            4
        );
        assert_eq!(max_unavailable(&with_budget(IntOrString::Int(0)), 4), 1);
        assert_eq!(
            max_unavailable(&with_budget(IntOrString::String("x".into())), 4),
            1
        );
    }

    #[test]
    fn availability_waits_for_min_ready_seconds() {
        let ok = binding("a", "r2", Some(("r2", true)));
        assert!(available(&ok, 0, now()));
        assert!(available(&ok, 60, now() + chrono::Duration::seconds(3600)));
        assert!(!available(&ok, 86_400 * 365 * 10, now()));
        assert!(!available(
            &binding("a", "r2", Some(("r1", true))),
            0,
            now()
        ));
        assert!(!available(
            &binding("a", "r2", Some(("r2", false))),
            0,
            now()
        ));
        assert!(!available(&binding("a", "r2", None), 0, now()));
        let mut undated = ok;
        undated.status.as_mut().unwrap().last_transition_time = None;
        assert!(!available(&undated, 1, now()));
    }

    #[test]
    fn new_nodes_get_the_current_revision_and_gone_nodes_lose_their_binding() {
        let bindings = [
            binding("a", "r2", Some(("r2", true))),
            binding("gone", "r2", Some(("r2", true))),
        ];
        let plan = plan(
            &spec(Rules::default()),
            "r2",
            &nodes(&["a", "b"]),
            &bindings,
            now(),
        );
        assert_eq!(
            plan.create,
            [Create {
                node: "b".to_string(),
                held: None
            }]
        );
        assert_eq!(plan.delete, [1]);
        assert_eq!(plan.update, Vec::<usize>::new());
        assert_eq!(
            plan.counts,
            Counts {
                desired: 2,
                updated: 2,
                ready: 1,
                available: 1
            }
        );
    }

    #[test]
    fn a_rolling_update_moves_one_node_at_a_time_by_default() {
        let all = nodes(&["a", "b", "c"]);
        let s = spec(Rules::default());
        let mut bindings = vec![
            binding("a", "r1", Some(("r1", true))),
            binding("b", "r1", Some(("r1", true))),
            binding("c", "r1", Some(("r1", true))),
        ];
        let first = plan(&s, "r2", &all, &bindings, now());
        assert_eq!(first.update, [0]);
        assert_eq!(
            first.counts,
            Counts {
                desired: 3,
                updated: 1,
                ready: 2,
                available: 2
            }
        );

        // Assigned but not applied yet: the budget is used up.
        bindings[0] = binding("a", "r2", Some(("r1", true)));
        assert_eq!(
            plan(&s, "r2", &all, &bindings, now()).update,
            Vec::<usize>::new()
        );

        // A failed rule on the new revision holds the rollout.
        bindings[0] = binding("a", "r2", Some(("r2", false)));
        assert_eq!(
            plan(&s, "r2", &all, &bindings, now()).update,
            Vec::<usize>::new()
        );

        bindings[0] = binding("a", "r2", Some(("r2", true)));
        assert_eq!(plan(&s, "r2", &all, &bindings, now()).update, [1]);
    }

    #[test]
    fn a_larger_budget_moves_more_nodes_and_broken_nodes_go_first_for_free() {
        let all = nodes(&["a", "b", "c", "d"]);
        let bindings = [
            binding("a", "r1", Some(("r1", true))),
            binding("b", "r1", Some(("r1", true))),
            binding("c", "r1", Some(("r1", false))),
            binding("d", "r1", Some(("r1", true))),
        ];
        let two = plan(
            &with_budget(IntOrString::Int(2)),
            "r2",
            &all,
            &bindings,
            now(),
        );
        assert_eq!(two.update, [2, 0]);

        let everything = plan(
            &with_budget(IntOrString::String("100%".into())),
            "r2",
            &all,
            &bindings,
            now(),
        );
        assert_eq!(everything.update, [2, 0, 1, 3]);
        assert_eq!(everything.counts.updated, 4);
        assert_eq!(everything.counts.available, 0);
    }

    #[test]
    fn paused_holds_the_revision_even_for_new_nodes() {
        let mut s = spec(Rules::default());
        s.paused = true;
        let bindings = [
            binding("a", "r2", Some(("r2", true))),
            binding("b", "r1", Some(("r1", true))),
        ];
        let held = plan(&s, "r2", &nodes(&["a", "b", "c"]), &bindings, now());
        assert_eq!(held.update, Vec::<usize>::new());
        assert_eq!(
            held.create,
            [Create {
                node: "c".to_string(),
                held: Some(1)
            }]
        );
        assert_eq!(held.counts.updated, 1);

        let fresh = plan(&s, "r2", &nodes(&["a"]), &[], now());
        assert_eq!(fresh.create[0].held, None);
    }

    #[test]
    fn on_delete_updates_only_nodes_without_a_binding() {
        let mut s = spec(Rules::default());
        s.strategy.type_ = StrategyType::OnDelete;
        let bindings = [binding("a", "r1", Some(("r1", true)))];
        let plan = plan(&s, "r2", &nodes(&["a", "b"]), &bindings, now());
        assert_eq!(plan.update, Vec::<usize>::new());
        assert_eq!(plan.create.len(), 1);
        assert_eq!(plan.counts.updated, 1);
    }
}
