# Unused volume identification

How the addon reports [PersistentVolumeClaims][k8s-pvc] and [PersistentVolumes][k8s-pv] no workload is using, and how to read the report. It never deletes anything.

A third loop lists the cluster's
PersistentVolumeClaims, PersistentVolumes, [Pods][k8s-pod], and [StatefulSets][k8s-statefulset] and reports
every claim and volume no workload is using, as Prometheus metrics and as
[annotations][k8s-annotations] on the object itself:

```console
$ kubectl get pvc -n legacy uploads \
    -o jsonpath='{.metadata.annotations}' | jq
{
  "external-ebs-autoresizer/unused": "true",
  "external-ebs-autoresizer/unused-since": "2026-08-20T03:11:00Z",
  "external-ebs-autoresizer/unused-reason": "no_consumer_pod",
  "external-ebs-autoresizer/unused-days": "7",
  "external-ebs-autoresizer/volume-id": "vol-0fedcba9876543210",
  "external-ebs-autoresizer/unused-observed-at": "2026-08-27T09:00:00Z"
}
```

The scanner only ever identifies. It never deletes a claim or a volume, and it
has no permission to: its [ClusterRole][k8s-rbac] grants list and patch, never delete. That
is deliberate. "Unused" is an observation about the cluster's current state, not
a statement that the data is disposable, and a claim kept for a quarterly job
looks identical from here to one nobody will ever read again. An operator reviews
the annotation and decides.

Run `external-ebs-autoresizer unused` for the same report as a table without
enabling the loop. See [cli.md](cli.md#unused).

## What counts as unused

A claim is unused when no live Pod mounts it. A Pod in a terminal phase is not a
consumer: a completed Job's Pod object outlives its run by hours and mounts
nothing, so counting it would hide exactly the claim this loop exists to surface.

| Reason | What it is |
|--------|------------|
| `no_consumer_pod` | A bound claim no live Pod mounts. The ordinary leak: the workload is gone, the claim is not |
| `statefulset_scaled_down` | A `volumeClaimTemplate` claim whose ordinal is outside its StatefulSet's replica range. Scaling down deliberately leaves these behind, which is what makes them the leak that survives longest unnoticed |
| `unbound` | A claim that never bound and has no Pod to trigger binding. It costs nothing yet, and nothing is coming to consume it |
| `released` | A volume whose claim was deleted under a `Retain` reclaim policy. Kubernetes will never reuse it and never delete it, so the EBS volume outlives every trace of the workload |
| `available` | A volume that has never been claimed |
| `failed` | A volume whose automatic reclamation failed |
| `missing_claim` | A volume still `Bound` to a claim that does not exist, or exists with a different UID (deleted and recreated under the same name) |
| `bound_to_unused_claim` | A volume bound to a claim that is itself unused. Reported separately from the claim so a report by volume is complete on its own, and so the capacity is counted where it is provisioned |

A `volumeClaimTemplate` claim **inside** its StatefulSet's replica range is in
use even with no Pod at all: there is no Pod between a delete and the next
schedule, and every rolling update passes through that gap. Matching is by the
generated `<template>-<statefulset>-<ordinal>` name, since the StatefulSet
controller sets no owner reference on the claims it generates.

## The grace period

Nothing is reported until it has been continuously unused for 24 hours. Most of
what a single pass sees as unused is a workload between two Pods, so a shorter
threshold reports mostly churn.

The clock lives in the object's own `unused-since` annotation rather than in
memory, so it survives a restart of the controller. That is why annotations are
written from the first pass that sees an object unused, while metrics and logs
wait for the threshold: if the annotation waited too, the clock would never
start. An object that comes back into use has every one of these keys removed on
the next pass, so a stale mark never outlives the condition that produced it.

## Knowing the scan is running

Every findings gauge is reset and republished on each pass, so all of them hold
their last values for as long as the process lives. A scanner that stopped an
hour ago and a cluster with nothing left to clean up read identically. Four
series exist so that case is not silent:

| Metric | What it answers |
|--------|-----------------|
| `external_ebs_autoresizer_unused_scan_total` | Is the loop ticking at all. Raised before the pass runs, so it counts attempts, not outcomes |
| `external_ebs_autoresizer_unused_scan_failure_total` | How many passes ended in an error. Distinct from `error_total{stage="pv_annotate"}`, which is a per-object failure inside a pass that still succeeds |
| `external_ebs_autoresizer_unused_scan_last_success_timestamp_seconds` | How current the report is. Alert on `time()` minus this, not on a boolean: a boolean set at startup stays `1` while the loop is wedged |
| `external_ebs_autoresizer_leader` | Which replica is actually running the loops. A follower publishes no activity by design, so a liveness alert that ignores this fires on every non-leader |

See [metrics.md](metrics.md) for the alert expression.

## No configuration

The loop has no settings. Its cadence (1 hour), its threshold (24 hours), what
counts as a consumer, how StatefulSet replica slots are treated, and which
annotation keys carry the verdict are all constants, for the same reason the
throughput recommender's decision tunables are: they are properties of how
Kubernetes behaves rather than per-cluster judgement calls, and every one of them
would only be a way to configure the scan into reporting nothing. Every namespace
is in scope. The startup logs print the effective values, since the mounted
config file says nothing about them.

`dryRun` is the one global switch it honors, and it suppresses the annotations
and the [Events][k8s-events]. The scan itself still runs and still reports, since reading the
cluster is not a mutation.

The addon's ClusterRole therefore always carries `list` on `pods` and
`statefulsets` plus `get`, `list`, and `patch` on `persistentvolumeclaims` and
`persistentvolumes`. It needs no AWS permissions: the EBS volume ID comes from
the PersistentVolume's `spec.csi.volumeHandle`, not from an EC2 call. Outside a
cluster (running the binary locally) the loop reports that it cannot start and
the resize loop runs on alone.

## Kubernetes Events

The verdict is also published as an Event against the object itself, so it shows
up next to the object's own history:

```console
$ kubectl describe pvc -n legacy uploads
...
Events:
  Type    Reason                Age                  From                       Message
  ----    ------                ----                 ----                       -------
  Normal  UnusedVolumeDetected  55m (x3 over 2h55m)  external-ebs-autoresizer   Unused for 7 days (no_consumer_pod), holding 20Gi on vol-0fedcba9876543210. Nothing was deleted. Review and remove it manually if the data is no longer needed.
```

Two moments are worth an Event, and they are shaped differently:

| Reason | When | Shape |
|--------|------|-------|
| `UnusedVolumeDetected` | Every pass, for every reported finding | A standing state. The recorder aggregates repeats into one Event and raises its count, so the cost is one Event object per finding rather than one per pass |
| `UnusedVolumeCleared` | Once, on the pass that erases the mark | A transition. It fires only when that pass actually erased something, so an object that never carried a mark reports nothing |

Both are `Normal`, not `Warning`. Nothing about the object is broken: an unused
claim is a cost observation, and a cluster with a hundred of them would drown the
Warnings that do mean something is failing.

A finding below the 24-hour threshold emits nothing, for the same reason it is not
exported. A dry run emits nothing either: an Event is a cluster write like the
annotation it accompanies.

Events land in the claim's own namespace, and in `default` for a
cluster-scoped PersistentVolume, the same as the [kubelet][k8s-kubelet]'s own [Node][k8s-node] Events. That
is why the grant lives in the ClusterRole rather than in the release-namespaced
Role.

One interaction to know about: the API server expires Events after
`--event-ttl` (one hour by default), while the standing `UnusedVolumeDetected`
Event is only refreshed once per scan, and the scan runs hourly. The two
therefore race, so an Event can briefly lapse between passes. The annotation and
the metrics do not expire and are the durable record.

See [designs/unused-volume-identification.md](designs/unused-volume-identification.md)
for why the loop identifies rather than deletes, where the grace-period clock
lives, and what it deliberately leaves out. See [metrics.md](metrics.md)
for the exported series and example queries, including the one that totals the
GiB a cluster is paying for and not using.

[k8s-pvc]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/#persistentvolumeclaims
[k8s-pv]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/
[k8s-statefulset]: https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/
[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-rbac]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#role-and-clusterrole
[k8s-events]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/
[k8s-kubelet]: https://kubernetes.io/docs/reference/command-line-tools-reference/kubelet/
[k8s-node]: https://kubernetes.io/docs/concepts/architecture/nodes/
