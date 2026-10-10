# Node throughput recommendations

How to read and apply the gp3 throughput recommendations the addon publishes on [Nodes][k8s-node]. See the design documents linked below for the decision rules.

Disabled by default. When enabled, a second loop recommends a gp3 throughput (and
the IOPS it requires) for every **in-cluster Kubernetes Node** and writes the result
as Node [annotations][k8s-annotations]:

```console
$ kubectl get node ip-10-0-1-5.ap-northeast-2.compute.internal \
    -o jsonpath='{.metadata.annotations.external-ebs-autoresizer/throughput-recommendation}'
increase
```

The recommender itself never modifies a volume. The demand signal comes from node
exporter counters in Prometheus or Mimir, not CloudWatch, because CloudWatch's
1-minute EBS granularity averages away the bursts a throughput recommendation exists
to catch. An operator reviews the annotation and applies it with
`aws ec2 modify-volume`.

This targets the opposite instance set from the resize loop, which excludes EKS
nodes by default. The two loops share only the process, the [leader election][k8s-leader-election], and the
AWS client.

## Applying recommendations on resize

When the recommender is enabled, the resize loop also folds a fresh **increase**
recommendation into a volume modification it is already making for a size
expansion. EC2 allows one modification per volume per 6 hours, so the combined
request spends the same slot the size change would have spent alone; a
throughput-only change is never issued. This only ever fires on volumes that are in
both loops' scope (EKS nodes matched by a resize policy with `excludeEKSNodes:
false`).

Guardrails, none of them configurable:

- Only an increase is applied. A decrease is never piggybacked: it would cut
  bandwidth at the exact moment the instance is busy enough to fill its disk.
- The recommendation must be fresh (observed within two recommender intervals),
  or it is ignored.
- If the combined request fails, the resize retries size-only, so the piggyback
  can never take down the urgent operation. The outcome is visible in
  `external_ebs_autoresizer_throughput_apply_total{result}`.

Enabling the recommender is the opt-in; set
`throughputRecommendation.applyOnResize: false` to keep recommendations
advisory-only annotations without losing them. See
[designs/throughput-apply-on-resize.md](designs/throughput-apply-on-resize.md)
for the hand-off architecture, the apply rules, and the Node events.

See [designs/ebs-throughput-recommendation.md](designs/ebs-throughput-recommendation.md)
for the annotation schema, the decision rules, the PromQL, the added RBAC and IAM,
and why single-volume nodes are the supported case.

[k8s-node]: https://kubernetes.io/docs/concepts/architecture/nodes/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-leader-election]: https://kubernetes.io/docs/concepts/architecture/leases/#leader-election
