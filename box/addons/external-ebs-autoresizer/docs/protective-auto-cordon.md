# Protective auto-cordon

What the [protective auto-cordon][k8s-cordon] does to a [Node][k8s-node] and how to watch it.

It is a protective measure, not maintenance: the addon cordons a Node whose root disk is filling up so the [scheduler][k8s-scheduler] stops placing new [Pods][k8s-pod] on a disk that is about to run out, and lifts the cordon itself once the disk has room again.

## Activation

There is no switch. The protective auto-cordon is always on for every measured instance that maps to a Node, so it activates as soon as the addon resizes EKS nodes (`excludeEKSNodes: false`). Standalone EC2 instances have no Node and are never affected. With the chart default `excludeEKSNodes: true` no Node is resized, so none is cordoned.

The chart grants `get`, `list`, and `patch` on `nodes` (cordon and annotation) and `patch` on `nodes/status` (condition) whenever `excludeEKSNodes` is false.

The former `autoProtectiveCordon` key is gone. A config that still sets it fails to load, and the chart rejects it in `values.schema.json`.

### Startup logs

Two lines at startup tell whether it is active and what it found:

```json
{"timestamp":"2026-10-10T01:00:00.000000Z","level":"INFO","message":"Protective cordon enabled: ...","enabled":true,"annotation":"external-ebs-autoresizer/protective-cordon","condition":"ProtectiveCordon","dry_run":false}
{"timestamp":"2026-10-10T01:00:00.350000Z","level":"INFO","message":"Protective cordon monitoring started","nodes_detected":42,"held_by_addon":1,"held_nodes":"[\"ip-10-0-1-5.ap-northeast-2.compute.internal\"]","cordoned_by_others":2,"condition_out_of_sync":0}
```

| Field | Meaning |
| --- | --- |
| `enabled` | `false` with a `reason` when `excludeEKSNodes` is true or there is no in-cluster access |
| `nodes_detected` | EC2-backed Nodes an instance can map to |
| `held_by_addon`, `held_nodes` | Nodes currently cordoned by the addon, carried over from a previous run |
| `cordoned_by_others` | Nodes cordoned by an operator, a drain, or another controller, which the addon leaves alone |
| `condition_out_of_sync` | Nodes whose `ProtectiveCordon` condition disagrees with the mark, rewritten on the first pass |

A failed startup list logs an error instead, usually a missing `nodes` grant. It is not fatal: every pass lists again.

## Behavior

- **Cordon**: when a measured instance maps to a Node (by [`spec.providerID`][k8s-node-spec]) and its root usage is at or above the effective `usageThresholdPercent`, the Node gets `spec.unschedulable: true` and the `external-ebs-autoresizer/protective-cordon` [annotation][k8s-annotations] (the cordon time). This happens before the resize, so it also covers the cooldown and max-size skips, the cases where the volume cannot grow in time.
- **Uncordon**: once usage is back under the threshold (right after a verified resize, or on any later pass), the addon removes `spec.unschedulable` and the annotation.
- **Condition**: the `ProtectiveCordon` [Node condition][k8s-node-conditions] mirrors the annotation. It is `True` with reason `ProtectiveCordonApplied` while the addon holds the cordon. It turns `False` with reason `ProtectiveCordonReleased` once the Node is schedulable again with usage back under the threshold, or with reason `ProtectiveCordonMarkRemoved` when someone else removed the annotation. Nodes the addon never cordoned do not carry it.
- **Ownership**: only a cordon carrying the annotation is ever lifted. A Node already cordoned by an operator, a [drain][k8s-drain], or another controller is left alone. A Node someone uncordoned by hand just has the annotation dropped.
- **Scope**: only EKS nodes have a Node object, so it applies only with `excludeEKSNodes: false`. Running Pods are never [evicted][k8s-eviction], and a `paused` policy is never measured, so its Nodes are neither cordoned nor uncordoned.
- **Dry run**: `dryRun: true` logs what would be cordoned or uncordoned and changes nothing.

## Node condition

The condition shows next to the kubelet's own conditions in [`kubectl describe node`][k8s-kubectl-describe]:

```console
$ kubectl describe node ip-10-0-1-5.ap-northeast-2.compute.internal
...
Conditions:
  Type              Status  LastHeartbeatTime     LastTransitionTime    Reason                   Message
  ----              ------  -----------------     ------------------    ------                   -------
  ProtectiveCordon  True    Fri, 09 Oct 2026 ...  Fri, 09 Oct 2026 ...  ProtectiveCordonApplied  external-ebs-autoresizer has cordoned the node for high root filesystem usage
  MemoryPressure    False   ...
```

List every Node the addon currently holds:

```console
$ kubectl get nodes -o jsonpath='{range .items[?(@.status.conditions[?(@.type=="ProtectiveCordon")].status=="True")]}{.metadata.name}{"\n"}{end}'
```

The condition is written with a [strategic merge patch][k8s-strategic-merge-patch] on the `status` subresource, which merges by condition `type`, so the kubelet's conditions are never touched. It is written only when it disagrees with the annotation, not every pass, so `lastHeartbeatTime` is the time of the last change.

## Kubernetes Events

Each change emits a Node [Event][k8s-events]: `ProtectiveCordonApplied` as Warning and `ProtectiveCordonReleased` as Normal, visible in [`kubectl describe node`][k8s-kubectl-describe].

```console
$ kubectl describe node ip-10-0-1-5.ap-northeast-2.compute.internal
...
Events:
  Type     Reason                    Age   From                      Message
  ----     ------                    ----  ----                      -------
  Warning  ProtectiveCordonApplied   12m   external-ebs-autoresizer  Protective cordon applied by external-ebs-autoresizer because root filesystem usage 85% reached the 80% threshold.
  Normal   ProtectiveCordonReleased  4m    external-ebs-autoresizer  Protective cordon released by external-ebs-autoresizer because root filesystem usage 62% is back under the 80% threshold.
```

Like the kubelet's own Node Events, they are stored in the `default` namespace, since a Node has none of its own:

```console
$ kubectl get events -n default --field-selector reason=ProtectiveCordonApplied
```

## Metrics

Each cordon and uncordon attempt counts in `external_ebs_autoresizer_protective_cordon_total{action,result}`. See [metrics.md](metrics.md#external_ebs_autoresizer_protective_cordon_total).

## Lifting a cordon by hand

To lift a protective cordon by hand, run [`kubectl uncordon <node>`][k8s-kubectl-uncordon]. The addon drops its annotation and sets the condition to `False` on the next pass that measures usage under the threshold. Switching to `excludeEKSNodes: true` stops the addon from reading Nodes, so uncordon remaining Nodes by hand.

## Design

See [designs/protective-auto-cordon.md](designs/protective-auto-cordon.md) for why it cordons instead of tainting or draining, the ownership mark, the threshold choice, and failure behavior.

[k8s-cordon]: https://kubernetes.io/docs/concepts/architecture/nodes/#manual-node-administration
[k8s-node]: https://kubernetes.io/docs/concepts/architecture/nodes/
[k8s-scheduler]: https://kubernetes.io/docs/concepts/scheduling-eviction/kube-scheduler/
[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-node-spec]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/node-v1/#NodeSpec
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-drain]: https://kubernetes.io/docs/tasks/administer-cluster/safely-drain-node/
[k8s-eviction]: https://kubernetes.io/docs/concepts/scheduling-eviction/node-pressure-eviction/
[k8s-kubectl-describe]: https://kubernetes.io/docs/reference/kubectl/generated/kubectl_describe/
[k8s-events]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/
[k8s-kubectl-uncordon]: https://kubernetes.io/docs/reference/kubectl/generated/kubectl_uncordon/
[k8s-node-conditions]: https://kubernetes.io/docs/reference/node/node-status/#condition
[k8s-strategic-merge-patch]: https://kubernetes.io/docs/tasks/manage-kubernetes-objects/update-api-object-kubectl-patch/#use-a-strategic-merge-patch-to-update-a-deployment
