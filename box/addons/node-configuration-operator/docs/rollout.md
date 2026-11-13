# Rollout

A NodeConfiguration rolls a change out the way a Deployment does. The strategy is set per resource, so a strict baseline can move one node at a time while a low-risk one moves faster, even though one agent pod per node applies them all.

## Revisions

The interval and the rules together are a revision, named by a 10-character hash shown in `status.revision`. Target and strategy fields are not part of it.

| Deployment | NodeConfiguration |
| --- | --- |
| Pod template hash | Revision hash of `rules`, `intervalSeconds`, `dryRun`, `group`, and `priority` |
| Pod | NodeConfigurationBinding, one per node |
| Replacing a Pod | Writing the new revision into the binding of a node |
| Ready Pod | Binding whose last run applied its revision with no failed rule |

The agent DaemonSet is not restarted by a rule change. The controller rewrites bindings, and each agent applies the binding it is handed. Only a change of the targets restarts the agents, because it changes their node affinity.

## Strategy fields

| Field | Default | Description |
| --- | --- | --- |
| `spec.strategy.type` | `RollingUpdate` | `RollingUpdate` or `OnDelete` |
| `spec.strategy.rollingUpdate.maxUnavailable` | `1` | Nodes that may be unavailable during the rollout, as a count or a percentage that rounds up |
| `spec.minReadySeconds` | `0` | Seconds a node must stay configured before it counts as available |
| `spec.paused` | `false` | Holds a new revision back |
| `spec.progressDeadlineSeconds` | `600` | Seconds without progress before the rollout is reported as stalled |

```yaml
spec:
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 10%
  minReadySeconds: 60
  progressDeadlineSeconds: 900
```

## How the budget is spent

A node is available when its binding reports the assigned revision as applied with no failed rule, for at least `minReadySeconds`. On every pass the controller counts the unavailable nodes and moves old-revision nodes to the current revision until the count reaches `maxUnavailable`.

- A node that is already unavailable moves first and costs no budget, so a fix reaches a broken node without waiting.
- A node that joins gets the current revision right away. It counts as unavailable until its first run.
- A node that fails a rule of the new revision stays unavailable and keeps its share of the budget. With the default of 1 a broken rule reaches one node and the rollout stops there.

After `progressDeadlineSeconds` without progress the `Progressing` condition turns false with the reason `ProgressDeadlineExceeded`. The operator does not roll back by itself. Revert the spec, and the stuck node moves back first.

## Pause and resume

With `spec.paused: true` no binding is moved. Edit the rules as often as needed, then set `paused` back to false to roll the final revision out once. A node that joins during the pause gets the revision the other nodes still hold.

```bash
kubectl patch nodeconfiguration os-hardening --type merge -p '{"spec":{"paused":true}}'
```

## OnDelete

With `type: OnDelete` a node moves to the new revision only when its binding is deleted, so the order and the timing are yours. The controller recreates the binding with the current revision.

```bash
kubectl delete nodeconfigurationbinding os-hardening.<node>
```

## Watching a rollout

```console
$ kubectl get nodeconfigurations
NAME           DESIRED   UPDATED   CONFIGURED   FAILED   READY   ROLLOUT      AGE
os-hardening   6         2         5            1        False   RollingOut   3d
```

| Column | Source | Meaning |
| --- | --- | --- |
| Desired | `status.desiredNodes` | Nodes the target selects |
| Updated | `status.updatedNodes` | Nodes assigned the current revision |
| Configured | `status.configuredNodes` | Nodes whose last run applied their assigned revision with no failed rule |
| Failed | `status.failedNodes` | Nodes whose last run failed at least one rule |
| Ready | `Ready` condition | True once every node is updated and available |
| Rollout | `Progressing` condition reason | See below |

| Progressing | Reason | Meaning |
| --- | --- | --- |
| True | RolloutComplete | The current revision is on every node |
| True | RollingOut | Nodes are being moved |
| True | WaitingForBindingDeletion | `OnDelete` and some nodes are on an older revision |
| Unknown | RolloutPaused | `spec.paused` holds a new revision back |
| False | ProgressDeadlineExceeded | No progress within the deadline |
| False | InvalidSpec | The spec failed a check and no node was moved |

`status.rules` and `status.failures` say which rule failed and on which nodes, see [Status](how-it-works.md#status). `kubectl get nodeconfigurationbindings -l node-configuration-operator.sh/configuration=os-hardening` shows the revision and the result node by node.

## Agent upgrades

A new image replaces the agent pods under `agent.updateStrategy` of the chart. That is independent of any NodeConfiguration: a restarted agent applies the bindings of its node again and reports the same revisions.
