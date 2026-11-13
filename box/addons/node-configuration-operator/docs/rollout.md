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
| `spec.strategy.rollingUpdate.cordon` | `false` | Cordons a node while it moves to a new revision, see [Cordon](#cordon) |
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

A node is available when its binding reports the assigned revision as applied with no failed rule, for at least `minReadySeconds`. A failure of a rule with `failurePolicy: Ignore` does not count. On every pass the controller counts the unavailable nodes and moves old-revision nodes to the current revision until the count reaches `maxUnavailable`.

- A node that is already unavailable moves first and costs no budget, so a fix reaches a broken node without waiting.
- A node that joins gets the current revision right away. It counts as unavailable until its first run.
- A node that fails a rule of the new revision stays unavailable and keeps its share of the budget. With the default of 1 a broken rule reaches one node and the rollout stops there.

After `progressDeadlineSeconds` without progress the `Progressing` condition turns false with the reason `ProgressDeadlineExceeded`. The operator does not roll back by itself. Revert the spec, and the stuck node moves back first.

## Cordon

With `spec.strategy.rollingUpdate.cordon: true` the controller cordons a node right before it writes the new revision into its binding, and uncordons it once the node is available again, which includes `minReadySeconds`. No new Pod is scheduled on a node whose configuration is changing. Running Pods stay, since the node is not drained.

```yaml
spec:
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 1
      cordon: true
  minReadySeconds: 30
```

- Only a move to a new revision cordons. The run on every interval and the first run on a node that joins do not, and neither does a dry run.
- Only the nodes a rollout moves are cordoned, so the `maxUnavailable` budget bounds how many healthy nodes are cordoned at a time.
- A node that fails a rule of the new revision stays cordoned until a later run or a reverted spec configures it.
- A node that was already cordoned is left as it is and is not uncordoned afterwards.
- A node that several NodeConfigurations move at once is uncordoned after the last of them is configured. The holders are listed in the `node-configuration-operator.sh/cordoned-by` annotation of the Node.
- The node is also uncordoned when it leaves the target, when `cordon` is turned off, and when the NodeConfiguration is deleted.
- Uncordoning a node by hand during a rollout is respected. The controller does not cordon it again for the same revision.

Every cordon and uncordon is recorded as an Event on the Node, so `kubectl describe node` shows who changed the node and why.

```console
$ kubectl events --for node/ip-10-0-1-23.ap-northeast-2.compute.internal
LAST SEEN   TYPE     REASON     OBJECT                                            MESSAGE
2m          Normal   Cordon     Node/ip-10-0-1-23.ap-northeast-2.compute.internal   Cordoned the node while NodeConfiguration os-hardening applies revision 5f8c2d1a9b
84s         Normal   Uncordon   Node/ip-10-0-1-23.ap-northeast-2.compute.internal   Uncordoned the node, NodeConfiguration os-hardening is configured on it
```

| Uncordon message ends with | Cause |
| --- | --- |
| `is configured on it` | The node applied its revision and stayed configured for `minReadySeconds` |
| `no longer targets it` | The target changed or the node was excluded |
| `no longer cordons nodes` | `cordon` was turned off, the strategy changed, or `dryRun` was turned on |
| `was deleted` | The NodeConfiguration was deleted |

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
| Failed | `status.failedNodes` | Nodes whose last run failed at least one rule, ignored failures aside |
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

## Conclusion

A rule change is a new revision that moves through the nodes under the budget of its own NodeConfiguration, and a failed rule stops it where it is. Cordon protects a node while it moves, pause and OnDelete hand the timing to you, and the `Progressing` condition says where a rollout stands.
