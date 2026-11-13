//! Derives the `NodeConfiguration` status from the counts of its bindings,
//! the way a `Deployment` reports the rollout of its current `ReplicaSet`.

use chrono::{DateTime, SecondsFormat, Utc};

use super::results::Results;
use super::rollout::Counts;
use crate::crd::{Condition, NodeConfiguration, NodeConfigurationStatus, StrategyType};

pub const READY: &str = "Ready";
pub const PROGRESSING: &str = "Progressing";
const ROLLING_OUT: &str = "RollingOut";
const DEADLINE_EXCEEDED: &str = "ProgressDeadlineExceeded";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation {
    Invalid(Vec<String>),
    Rollout {
        /// The revision the spec describes.
        revision: String,
        counts: Counts,
        results: Results,
    },
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn find<'a>(status: Option<&'a NodeConfigurationStatus>, type_: &str) -> Option<&'a Condition> {
    status.and_then(|s| s.conditions.iter().find(|c| c.type_ == type_))
}

fn condition(
    previous: Option<&Condition>,
    type_: &str,
    (status, reason, message): (&str, &str, String),
    now: &str,
    last_update_time: Option<String>,
) -> Condition {
    let unchanged =
        previous.filter(|c| c.status == status && c.reason == reason && c.message == message);
    Condition {
        type_: type_.to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        last_update_time: last_update_time
            .or_else(|| unchanged.map(|c| c.last_update_time.clone()))
            .unwrap_or_else(|| now.to_string()),
        last_transition_time: previous
            .filter(|c| c.status == status)
            .map_or_else(|| now.to_string(), |c| c.last_transition_time.clone()),
        message,
    }
}

/// Whether a rollout is under way, which the controller checks again soon to
/// catch the progress deadline.
pub fn rolling_out(status: &NodeConfigurationStatus) -> bool {
    find(Some(status), PROGRESSING).is_some_and(|c| c.reason == ROLLING_OUT)
}

/// The `Progressing` condition and, while a rollout runs, when it last moved.
fn progressing(
    obj: &NodeConfiguration,
    revision: &str,
    counts: Counts,
    complete: bool,
    now: DateTime<Utc>,
) -> ((&'static str, &'static str, String), Option<String>) {
    let Counts {
        desired,
        updated,
        available,
        ..
    } = counts;
    if complete {
        let message = format!("Revision {revision} is on all {desired} nodes");
        return (("True", "RolloutComplete", message), None);
    }
    if obj.spec.paused {
        let message = format!("Revision {revision} is held back while spec.paused is true");
        return (("Unknown", "RolloutPaused", message), None);
    }
    if obj.spec.strategy.type_ == StrategyType::OnDelete {
        let message = format!(
            "{updated} of {desired} nodes run revision {revision}, the others update when their binding is deleted"
        );
        return (("True", "WaitingForBindingDeletion", message), None);
    }

    let previous = obj.status.as_ref();
    let stalled_since = find(previous, PROGRESSING)
        .filter(|c| c.reason == ROLLING_OUT || c.reason == DEADLINE_EXCEEDED)
        .filter(|_| {
            previous.is_some_and(|s| {
                s.revision.as_deref() == Some(revision)
                    && s.updated_nodes == updated
                    && s.available_nodes == available
            })
        })
        .and_then(|c| DateTime::parse_from_rfc3339(&c.last_update_time).ok())
        .map_or(now, |time| time.with_timezone(&Utc));
    let deadline = i64::from(obj.spec.progress_deadline_seconds);
    let condition = if (now - stalled_since).num_seconds() > deadline {
        (
            "False",
            DEADLINE_EXCEEDED,
            format!(
                "No progress for {deadline} seconds, {updated} of {desired} nodes run revision {revision} and {available} are available"
            ),
        )
    } else {
        (
            "True",
            ROLLING_OUT,
            format!(
                "{updated} of {desired} nodes run revision {revision}, {available} are available"
            ),
        )
    };
    (condition, Some(timestamp(stalled_since)))
}

pub fn compute(
    obj: &NodeConfiguration,
    observation: &Observation,
    now: DateTime<Utc>,
) -> NodeConfigurationStatus {
    let previous = obj.status.as_ref();
    let now_text = timestamp(now);
    let build = |ready, progressing, last_progress| {
        vec![
            condition(find(previous, READY), READY, ready, &now_text, None),
            condition(
                find(previous, PROGRESSING),
                PROGRESSING,
                progressing,
                &now_text,
                last_progress,
            ),
        ]
    };

    let (revision, counts, results) = match observation {
        Observation::Invalid(problems) => {
            let message = problems.join(", ");
            return NodeConfigurationStatus {
                observed_generation: obj.metadata.generation,
                conditions: build(
                    ("False", "InvalidSpec", message.clone()),
                    ("False", "InvalidSpec", message),
                    None,
                ),
                ..previous.cloned().unwrap_or_default()
            };
        }
        Observation::Rollout {
            revision,
            counts,
            results,
        } => (revision, *counts, results),
    };

    let Counts {
        desired,
        updated,
        ready,
        available,
    } = counts;
    let complete = updated == desired && available == desired;

    let ready_condition = if complete && desired == 0 {
        (
            "True",
            "NoMatchingNodes",
            "No managed node group node matches the target".to_string(),
        )
    } else if complete {
        (
            "True",
            "Configured",
            format!("All {desired} nodes are configured on revision {revision}"),
        )
    } else {
        (
            "False",
            "NodesPending",
            format!(
                "{updated} of {desired} nodes run revision {revision}, {available} are available"
            ),
        )
    };

    let (progressing, last_progress) = progressing(obj, revision, counts, complete, now);

    NodeConfigurationStatus {
        observed_generation: obj.metadata.generation,
        desired_nodes: desired,
        updated_nodes: updated,
        configured_nodes: ready,
        available_nodes: available,
        failed_nodes: results.failed_nodes,
        revision: Some(revision.clone()),
        rules: results.rules.clone(),
        failures: results.failures.clone(),
        conditions: build(ready_condition, progressing, last_progress),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::Rules;
    use crate::crd::testing::resource;
    use chrono::TimeZone as _;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + seconds, 0).unwrap()
    }

    fn agents(desired: i32, updated: i32, available: i32) -> Observation {
        Observation::Rollout {
            revision: "rev2".to_string(),
            counts: Counts {
                desired,
                updated,
                ready: available,
                available,
            },
            results: Results::default(),
        }
    }

    fn reasons(status: &NodeConfigurationStatus) -> [(&str, &str); 2] {
        let get = |type_| {
            let c = find(Some(status), type_).unwrap();
            (c.status.as_str(), c.reason.as_str())
        };
        [get(READY), get(PROGRESSING)]
    }

    #[test]
    fn a_finished_rollout_is_ready() {
        let obj = resource(Rules::default());
        let status = compute(&obj, &agents(3, 3, 3), at(0));
        assert_eq!(
            reasons(&status),
            [("True", "Configured"), ("True", "RolloutComplete")]
        );
        assert_eq!(status.observed_generation, Some(2));
        assert_eq!(status.revision.as_deref(), Some("rev2"));
        assert_eq!(
            (
                status.desired_nodes,
                status.updated_nodes,
                status.configured_nodes,
                status.available_nodes
            ),
            (3, 3, 3, 3)
        );
        assert!(!rolling_out(&status));

        let empty = compute(&obj, &agents(0, 0, 0), at(0));
        assert_eq!(
            reasons(&empty),
            [("True", "NoMatchingNodes"), ("True", "RolloutComplete")]
        );
    }

    #[test]
    fn a_partial_rollout_is_pending() {
        let obj = resource(Rules::default());
        let status = compute(&obj, &agents(3, 0, 3), at(0));
        assert_eq!(status.updated_nodes, 0);
        assert_eq!(
            reasons(&status),
            [("False", "NodesPending"), ("True", "RollingOut")]
        );
        assert!(rolling_out(&status));
    }

    #[test]
    fn a_stalled_rollout_exceeds_the_deadline_and_recovers_on_progress() {
        let mut obj = resource(Rules::default());
        let started = compute(&obj, &agents(3, 1, 0), at(0));
        obj.status = Some(started);

        let waiting = compute(&obj, &agents(3, 1, 0), at(600));
        assert_eq!(reasons(&waiting)[1], ("True", "RollingOut"));
        assert_eq!(
            find(Some(&waiting), PROGRESSING).unwrap().last_update_time,
            timestamp(at(0))
        );
        obj.status = Some(waiting);

        let stalled = compute(&obj, &agents(3, 1, 0), at(601));
        assert_eq!(reasons(&stalled)[1], ("False", "ProgressDeadlineExceeded"));
        let progressing = find(Some(&stalled), PROGRESSING).unwrap();
        assert_eq!(progressing.last_transition_time, timestamp(at(601)));
        obj.status = Some(stalled);

        let still = compute(&obj, &agents(3, 1, 0), at(900));
        assert_eq!(reasons(&still)[1], ("False", "ProgressDeadlineExceeded"));
        assert_eq!(
            find(Some(&still), PROGRESSING)
                .unwrap()
                .last_transition_time,
            timestamp(at(601))
        );
        obj.status = Some(still);

        let moved = compute(&obj, &agents(3, 1, 1), at(901));
        assert_eq!(reasons(&moved)[1], ("True", "RollingOut"));
        obj.status = Some(moved);

        let done = compute(&obj, &agents(3, 3, 3), at(950));
        assert_eq!(
            reasons(&done),
            [("True", "Configured"), ("True", "RolloutComplete")]
        );
        let ready = find(Some(&done), READY).unwrap();
        assert_eq!(ready.last_transition_time, timestamp(at(950)));
    }

    #[test]
    fn paused_and_on_delete_have_no_deadline() {
        let mut obj = resource(Rules::default());
        obj.spec.paused = true;
        let paused = compute(&obj, &agents(3, 0, 3), at(0));
        assert_eq!(
            reasons(&paused),
            [("False", "NodesPending"), ("Unknown", "RolloutPaused")]
        );
        obj.status = Some(paused);
        let later = compute(&obj, &agents(3, 0, 3), at(100_000));
        assert_eq!(reasons(&later)[1], ("Unknown", "RolloutPaused"));
        assert_eq!(obj.status, Some(later));

        obj.spec.paused = false;
        obj.spec.strategy.type_ = StrategyType::OnDelete;
        let on_delete = compute(&obj, &agents(3, 1, 3), at(200_000));
        assert_eq!(
            reasons(&on_delete)[1],
            ("True", "WaitingForBindingDeletion")
        );
    }

    #[test]
    fn an_invalid_spec_keeps_the_last_counts() {
        let mut obj = resource(Rules::default());
        obj.status = Some(compute(&obj, &agents(3, 3, 3), at(0)));
        let invalid = Observation::Invalid(vec!["a".into(), "b".into()]);
        let status = compute(&obj, &invalid, at(10));
        assert_eq!(
            reasons(&status),
            [("False", "InvalidSpec"), ("False", "InvalidSpec")]
        );
        assert_eq!(status.conditions[0].message, "a, b");
        assert_eq!((status.desired_nodes, status.configured_nodes), (3, 3));
        assert_eq!(status.revision.as_deref(), Some("rev2"));
    }
}
