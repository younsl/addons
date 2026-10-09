# Protective auto-cordon

How to turn on the [protective auto-cordon][k8s-cordon] and what it does to a [Node][k8s-node].

Disabled by default. Set `autoProtectiveCordon: true` on `defaultPolicy` or on a policy's `resize` block to protect scheduling on in-cluster Kubernetes Nodes whose root disk is filling up. It is a protective measure, not maintenance: the addon cordons the Node so the [scheduler][k8s-scheduler] stops placing new [Pods][k8s-pod] on a disk that is about to run out, and lifts the cordon itself once the disk has room again.

- **Cordon**: when a measured instance maps to a Node (by [`spec.providerID`][k8s-node-spec]) and its root usage is at or above the effective `usageThresholdPercent`, the Node gets `spec.unschedulable: true` and the `external-ebs-autoresizer/protective-cordon` [annotation][k8s-annotations] (the cordon time). This happens before the resize, so it also covers the cooldown and max-size skips, the cases where the volume cannot grow in time.
- **Uncordon**: once usage is back under the threshold (right after a verified resize, or on any later pass), the addon removes `spec.unschedulable` and the annotation.
- **Ownership**: only a cordon carrying the annotation is ever lifted. A Node already cordoned by an operator, a [drain][k8s-drain], or another controller is left alone. A Node someone uncordoned by hand just has the annotation dropped.
- **Scope**: only EKS nodes have a Node object, so it needs `excludeEKSNodes: false` (the addon warns at startup otherwise). Running Pods are never [evicted][k8s-eviction], and a `paused` policy is never measured, so its Nodes are neither cordoned nor uncordoned.
- **Dry run**: `dryRun: true` logs what would be cordoned or uncordoned and changes nothing.

Each change emits a Node [Event][k8s-events] (`ProtectiveCordonApplied` as Warning, `ProtectiveCordonReleased` as Normal), visible in [`kubectl describe node`][k8s-kubectl-describe], and counts in `external_ebs_autoresizer_protective_cordon_total{action,result}`. The chart grants `list` and `patch` on `nodes` whenever any policy turns it on.

```console
$ kubectl describe node ip-10-0-1-5.ap-northeast-2.compute.internal
...
Events:
  Type     Reason                    Age   From                      Message
  ----     ------                    ----  ----                      -------
  Warning  ProtectiveCordonApplied   12m   external-ebs-autoresizer  Protective cordon applied by external-ebs-autoresizer: root filesystem usage 85% is at or above the 80% threshold, so no new Pods are scheduled here. It is lifted automatically once usage falls back under the threshold.
  Normal   ProtectiveCordonReleased  4m    external-ebs-autoresizer  Protective cordon released by external-ebs-autoresizer: root filesystem usage 62% is back under the 80% threshold.
```

Node Events are stored in the `default` namespace, so they can also be listed across the cluster:

```console
$ kubectl get events -n default --field-selector reason=ProtectiveCordonApplied
```

```yaml
excludeEKSNodes: false
policies:
  - name: eks-workers
    weight: 1
    instanceSelector:
      tags:
        eks:cluster-name: example
    resize:
      autoProtectiveCordon: true
```

To lift a protective cordon by hand, run [`kubectl uncordon <node>`][k8s-kubectl-uncordon]. The addon drops its annotation on the next pass. Turning `autoProtectiveCordon` off for a group keeps releasing that group's existing cordons as long as any policy still has it on. With it off everywhere the addon stops reading Nodes, so uncordon remaining Nodes by hand.

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
