//! Cordons a node while a `NodeConfiguration` moves it to a new revision, and
//! records every cordon and uncordon as an Event on the node.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::events::{Event, EventType};
use kube::{Resource as _, ResourceExt as _};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::Context;

/// Names of the `NodeConfiguration` resources that hold the cordon of a node,
/// comma separated.
pub const HOLDERS_ANNOTATION: &str = "node-configuration-operator.sh/cordoned-by";

/// Why a `NodeConfiguration` gives its cordon back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Configured,
    Untargeted,
    Disabled,
    Deleted,
}

impl Why {
    fn sentence(self, name: &str) -> String {
        match self {
            Self::Configured => format!("NodeConfiguration {name} is configured on it"),
            Self::Untargeted => format!("NodeConfiguration {name} no longer targets it"),
            Self::Disabled => format!("NodeConfiguration {name} no longer cordons nodes"),
            Self::Deleted => format!("NodeConfiguration {name} was deleted"),
        }
    }
}

pub fn holders(annotations: Option<&BTreeMap<String, String>>) -> BTreeSet<String> {
    annotations
        .and_then(|a| a.get(HOLDERS_ANNOTATION))
        .map_or_else(BTreeSet::new, |names| {
            names
                .split(',')
                .filter(|name| !name.is_empty())
                .map(ToString::to_string)
                .collect()
        })
}

fn cordoned(node: &Node) -> bool {
    node.spec
        .as_ref()
        .and_then(|spec| spec.unschedulable)
        .unwrap_or(false)
}

/// A merge patch of a node, and whether it changes `spec.unschedulable`.
#[derive(Debug, PartialEq)]
struct Change {
    patch: Value,
    flipped: bool,
}

// The resourceVersion makes the patch fail when another holder changed the
// node in between.
fn patch(node: &Node, held: &BTreeSet<String>, unschedulable: Option<bool>) -> Value {
    let names = held.iter().map(String::as_str).collect::<Vec<_>>();
    let annotation = (!names.is_empty()).then(|| names.join(","));
    let mut patch = json!({
        "metadata": {
            "resourceVersion": node.metadata.resource_version,
            "annotations": { HOLDERS_ANNOTATION: annotation },
        },
    });
    if let Some(unschedulable) = unschedulable {
        // Kubernetes stores a schedulable node without the field.
        patch["spec"] = json!({ "unschedulable": unschedulable.then_some(true) });
    }
    patch
}

/// Takes the cordon for `name`. A node that someone else cordoned is left
/// alone, so it stays theirs to uncordon.
fn hold(node: &Node, name: &str) -> Option<Change> {
    let mut held = holders(node.metadata.annotations.as_ref());
    let cordoned = cordoned(node);
    if held.contains(name) || (cordoned && held.is_empty()) {
        return None;
    }
    held.insert(name.to_string());
    Some(Change {
        patch: patch(node, &held, Some(true)),
        flipped: !cordoned,
    })
}

/// Gives the cordon of `name` back. The node is uncordoned once no holder is
/// left.
fn release(node: &Node, name: &str) -> Option<Change> {
    let mut held = holders(node.metadata.annotations.as_ref());
    if !held.remove(name) {
        return None;
    }
    let last = held.is_empty();
    Some(Change {
        patch: patch(node, &held, last.then_some(false)),
        flipped: last && cordoned(node),
    })
}

/// Applies a change to a node and returns the node when it was cordoned or
/// uncordoned by it. A node that is gone needs no change.
async fn change(
    ctx: &Context,
    node: &str,
    compute: impl Fn(&Node) -> Option<Change>,
) -> Result<Option<Node>, kube::Error> {
    let api: Api<Node> = Api::all(ctx.client.clone());
    let Some(current) = api.get_opt(node).await? else {
        return Ok(None);
    };
    let Some(change) = compute(&current) else {
        return Ok(None);
    };
    api.patch(node, &PatchParams::default(), &Patch::Merge(&change.patch))
        .await?;
    Ok(change.flipped.then_some(current))
}

async fn record(ctx: &Context, node: &Node, reason: &str, message: String) {
    let event = Event {
        type_: EventType::Normal,
        reason: reason.to_string(),
        note: Some(message),
        action: reason.to_string(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&event, &node.object_ref(&())).await {
        warn!(
            node = node.name_any(),
            reason, "Could not record the event on the node, {e}"
        );
    }
}

pub async fn acquire(
    ctx: &Context,
    node: &str,
    name: &str,
    revision: &str,
) -> Result<(), kube::Error> {
    if let Some(cordoned) = change(ctx, node, |current| hold(current, name)).await? {
        info!(name, node, revision, "Cordoned the node for the rollout");
        let message =
            format!("Cordoned the node while NodeConfiguration {name} applies revision {revision}");
        record(ctx, &cordoned, "Cordon", message).await;
    }
    Ok(())
}

pub async fn free(ctx: &Context, node: &str, name: &str, why: Why) -> Result<(), kube::Error> {
    if let Some(uncordoned) = change(ctx, node, |current| release(current, name)).await? {
        info!(name, node, reason = ?why, "Uncordoned the node");
        let message = format!("Uncordoned the node, {}", why.sentence(name));
        record(ctx, &uncordoned, "Uncordon", message).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::NodeSpec;

    fn node(unschedulable: bool, held: &str) -> Node {
        let mut node = Node::default();
        node.metadata.name = Some("a".to_string());
        node.metadata.resource_version = Some("5".to_string());
        if !held.is_empty() {
            node.metadata.annotations =
                Some([(HOLDERS_ANNOTATION.to_string(), held.to_string())].into());
        }
        node.spec = Some(NodeSpec {
            unschedulable: unschedulable.then_some(true),
            ..NodeSpec::default()
        });
        node
    }

    #[test]
    fn holders_are_read_from_the_annotation() {
        assert!(holders(None).is_empty());
        let node = node(true, "b,a,");
        assert_eq!(
            holders(node.metadata.annotations.as_ref()),
            ["a".to_string(), "b".to_string()].into()
        );
    }

    #[test]
    fn hold_cordons_a_schedulable_node() {
        let change = hold(&node(false, ""), "mng").unwrap();
        assert!(change.flipped);
        assert_eq!(
            change.patch,
            json!({
                "metadata": {
                    "resourceVersion": "5",
                    "annotations": { HOLDERS_ANNOTATION: "mng" },
                },
                "spec": { "unschedulable": true },
            })
        );
    }

    #[test]
    fn hold_leaves_a_node_someone_else_cordoned() {
        assert_eq!(hold(&node(true, ""), "mng"), None);
        assert_eq!(hold(&node(true, "mng"), "mng"), None);
    }

    #[test]
    fn hold_joins_another_holder_without_a_second_cordon() {
        let change = hold(&node(true, "other"), "mng").unwrap();
        assert!(!change.flipped);
        assert_eq!(
            change.patch["metadata"]["annotations"][HOLDERS_ANNOTATION],
            "mng,other"
        );
    }

    #[test]
    fn release_uncordons_once_the_last_holder_leaves() {
        let change = release(&node(true, "mng,other"), "mng").unwrap();
        assert!(!change.flipped);
        assert_eq!(
            change.patch,
            json!({
                "metadata": {
                    "resourceVersion": "5",
                    "annotations": { HOLDERS_ANNOTATION: "other" },
                },
            })
        );

        let change = release(&node(true, "mng"), "mng").unwrap();
        assert!(change.flipped);
        assert_eq!(
            change.patch,
            json!({
                "metadata": {
                    "resourceVersion": "5",
                    "annotations": { HOLDERS_ANNOTATION: null },
                },
                "spec": { "unschedulable": null },
            })
        );
    }

    #[test]
    fn release_reports_no_uncordon_for_a_node_uncordoned_by_hand() {
        let change = release(&node(false, "mng"), "mng").unwrap();
        assert!(!change.flipped);
        assert_eq!(release(&node(true, "other"), "mng"), None);
    }

    #[test]
    fn every_reason_reads_as_a_sentence() {
        assert_eq!(
            Why::Configured.sentence("mng"),
            "NodeConfiguration mng is configured on it"
        );
        assert_eq!(
            Why::Untargeted.sentence("mng"),
            "NodeConfiguration mng no longer targets it"
        );
        assert_eq!(
            Why::Disabled.sentence("mng"),
            "NodeConfiguration mng no longer cordons nodes"
        );
        assert_eq!(
            Why::Deleted.sentence("mng"),
            "NodeConfiguration mng was deleted"
        );
    }
}
