//! Aggregates what the agents reported in the bindings, so one look at the
//! `NodeConfiguration` shows which rule failed on how many nodes.

use std::collections::{BTreeMap, BTreeSet};

use crate::crd::binding::NodeConfigurationBinding;
use crate::crd::{NodeConfiguration, NodeFailure, RuleSummary};

/// The status lists only this many nodes. `failedNodes` carries the count.
pub const MAX_FAILURES: usize = 10;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    pub failed_nodes: i32,
    pub rules: Vec<RuleSummary>,
    pub failures: Vec<NodeFailure>,
}

fn count(summary: &mut RuleSummary, outcome: &str) {
    match outcome {
        "unchanged" => summary.unchanged += 1,
        "changed" => summary.changed += 1,
        "pending" => summary.pending += 1,
        "skipped" => summary.skipped += 1,
        "failed" => summary.failed += 1,
        _ => {}
    }
}

/// The results of the targeted `nodes`. Rule counts cover the nodes that ran
/// the current `revision`, because an older revision may hold other rules.
pub fn collect(
    obj: &NodeConfiguration,
    revision: &str,
    nodes: &BTreeSet<String>,
    bindings: &[NodeConfigurationBinding],
) -> Results {
    let mut summaries: Vec<RuleSummary> = obj
        .spec
        .rules
        .iter()
        .map(|rule| RuleSummary {
            name: rule.name.clone(),
            ..RuleSummary::default()
        })
        .collect();
    let index: BTreeMap<String, usize> = summaries
        .iter()
        .enumerate()
        .map(|(i, summary)| (summary.name.clone(), i))
        .collect();

    let mut failed_nodes = 0;
    let mut failures = Vec::new();
    for binding in bindings {
        let node = &binding.spec.node_name;
        let Some(status) = binding.status.as_ref().filter(|_| nodes.contains(node)) else {
            continue;
        };
        if status.failed_rules > 0 {
            failed_nodes += 1;
        }
        if !status.ready
            && let Some(message) = &status.message
        {
            failures.push(NodeFailure {
                node: node.clone(),
                message: message.clone(),
            });
        }
        let current = binding.spec.revision == revision
            && status.observed_revision.as_deref() == Some(revision);
        if current {
            for rule in &status.rules {
                if let Some(i) = index.get(&rule.name) {
                    count(&mut summaries[*i], &rule.outcome);
                }
            }
        }
    }
    failures.sort_by(|a, b| a.node.cmp(&b.node));
    failures.truncate(MAX_FAILURES);
    Results {
        failed_nodes,
        rules: summaries,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::binding::{
        NodeConfigurationBindingSpec, NodeConfigurationBindingStatus, RuleStatus,
    };
    use crate::crd::testing::resource;
    use crate::crd::{FileRule, NamedRule};

    fn rule(name: &str) -> NamedRule {
        NamedRule::file(
            name,
            FileRule {
                path: format!("/etc/{name}"),
                content: String::new(),
                mode: "0644".into(),
            },
        )
    }

    fn binding(
        node: &str,
        revision: &str,
        outcomes: &[(&str, &str)],
        message: Option<&str>,
    ) -> NodeConfigurationBinding {
        let failed = outcomes.iter().filter(|(_, o)| *o == "failed").count();
        let mut binding = NodeConfigurationBinding::new(
            &format!("mng.{node}"),
            NodeConfigurationBindingSpec {
                configuration: "mng".into(),
                node_name: node.into(),
                revision: revision.into(),
                group: None,
                priority: 0,
                dry_run: false,
                interval_seconds: 300,
                rules: Vec::new(),
            },
        );
        binding.status = Some(NodeConfigurationBindingStatus {
            observed_revision: Some(revision.into()),
            ready: failed == 0 && message.is_none(),
            failed_rules: i32::try_from(failed).unwrap(),
            message: message.map(Into::into),
            rules: outcomes
                .iter()
                .map(|(name, outcome)| RuleStatus {
                    name: (*name).into(),
                    outcome: (*outcome).into(),
                    message: None,
                })
                .collect(),
            last_transition_time: None,
        });
        binding
    }

    #[test]
    fn rules_are_counted_over_the_nodes_on_the_current_revision() {
        let obj = resource(vec![rule("motd"), rule("issue")]);
        let nodes: BTreeSet<String> = ["a", "b", "c", "d", "e"].map(String::from).into();
        let mut unreported = binding("e", "new", &[], None);
        unreported.status = None;
        let bindings = [
            binding(
                "a",
                "new",
                &[("motd", "unchanged"), ("issue", "changed")],
                None,
            ),
            binding(
                "b",
                "new",
                &[("motd", "failed"), ("issue", "skipped"), ("gone", "failed")],
                Some("rule motd failed, file /etc/motd, denied"),
            ),
            binding("c", "old", &[("motd", "failed")], Some("rule motd failed")),
            binding("d", "new", &[("motd", "pending")], Some("waiting for base")),
            unreported,
            binding("left", "new", &[("motd", "failed")], Some("not targeted")),
        ];
        let results = collect(&obj, "new", &nodes, &bindings);
        assert_eq!(results.failed_nodes, 2);
        assert_eq!(
            results.rules,
            vec![
                RuleSummary {
                    name: "motd".into(),
                    unchanged: 1,
                    pending: 1,
                    failed: 1,
                    ..RuleSummary::default()
                },
                RuleSummary {
                    name: "issue".into(),
                    changed: 1,
                    skipped: 1,
                    ..RuleSummary::default()
                },
            ]
        );
        let listed: Vec<(&str, &str)> = results
            .failures
            .iter()
            .map(|f| (f.node.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("b", "rule motd failed, file /etc/motd, denied"),
                ("c", "rule motd failed"),
                ("d", "waiting for base"),
            ]
        );
    }

    #[test]
    fn the_failure_list_is_capped() {
        let obj = resource(vec![rule("motd")]);
        let names: Vec<String> = (0..25).map(|i| format!("node-{i:02}")).collect();
        let nodes: BTreeSet<String> = names.iter().cloned().collect();
        let bindings: Vec<_> = names
            .iter()
            .rev()
            .map(|node| binding(node, "new", &[("motd", "failed")], Some("rule motd failed")))
            .collect();
        let results = collect(&obj, "new", &nodes, &bindings);
        assert_eq!(results.failed_nodes, 25);
        assert_eq!(results.failures.len(), MAX_FAILURES);
        assert_eq!(results.failures[0].node, "node-00");
        assert_eq!(results.rules[0].failed, 25);
    }
}
