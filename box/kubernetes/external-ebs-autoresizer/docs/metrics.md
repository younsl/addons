# Metrics

## Overview

This document explains the Prometheus metrics that external-ebs-autoresizer
exposes. It describes what each metric means, which labels it carries, and how
you can use it to watch the addon in production.

Read this if you are:

- A platform or DevOps engineer who runs this addon and wants to build
  dashboards or alerts.
- An on-call engineer who needs to check whether disk resizes are working.
- Anyone who wants to understand the numbers on the `/metrics` endpoint.

You do not need to read the source code to follow this document. Basic
familiarity with Prometheus and PromQL is enough.

## Background

The addon runs as a long-lived Deployment inside EKS. On a fixed interval it
scans standalone EC2 instances, measures their root disk usage, and grows the
root EBS volume when usage crosses a threshold. One full scan is called a
**reconcile pass**, and each instance inside a pass goes through several
**stages** in order:

1. `discover` find the target instances and their root volumes.
2. `measure` run `df` over SSM and read the root usage percent.
3. `cooldown` check that the volume is not inside the 6-hour modify window.
4. `modify` call `ec2:ModifyVolume` to grow the volume.
5. `wait` poll until the modification reaches the `optimizing` state.
6. `resize` extend the filesystem with `growpart` and `resize2fs`.

The addon publishes its metrics on the `/metrics` HTTP path. The default port is
`8081` and can be changed with `metricsPort` in the config file. Prometheus
scrapes that endpoint on its own schedule. All metric names share the prefix
`external_ebs_autoresizer_`.

A short reminder on metric types:

- A **gauge** is a value that can go up and down, like a temperature. It always
  reports the latest reading.
- A **counter** only goes up. It resets to zero when the process restarts. You
  usually look at how fast it grows with `rate()`, not at its raw value.

## Metrics

Every metric name follows the [Prometheus naming
conventions](https://prometheus.io/docs/practices/naming/) and is built from
three parts:

```
external_ebs_autoresizer_<subject>_<unit or suffix>
```

- `external_ebs_autoresizer_` is the application prefix (the Prometheus
  "namespace"). It scopes every metric to this addon, so names never collide
  with other exporters and `{__name__=~"external_ebs_autoresizer_.*"}` finds
  everything the addon exposes.
- `<subject>` says what is measured, for example `root_usage`, `root_volume_size`,
  or `resize`.
- The last part encodes the unit or the type convention: gauges end with their
  unit (`_percent`, `_gib`) or a plain noun (`_instances`), and counters always
  end with `_total`.

So `external_ebs_autoresizer_root_volume_size_gib` reads as: this addon's root
volume size, in GiB.

### external_ebs_autoresizer_root_usage_percent

- Type: Gauge
- Labels: `instance_id`, `device`, `volume_id`, `name`

The most recent root filesystem usage percent for one instance. The addon
updates this value every time it measures an instance during a reconcile pass.
A value of `85` means the root disk was 85% full at the last measurement.

The labels tell you exactly which disk the reading belongs to:

| Label | Meaning |
|-------|---------|
| `instance_id` | EC2 instance ID, for example `i-0abc123` |
| `device` | Root device name, for example `/dev/xvda` |
| `volume_id` | Root EBS volume ID, for example `vol-0abc123` |
| `name` | Value of the instance `Name` tag |

Use it to see which instances are close to filling up, and to confirm that usage
drops after a resize.

### external_ebs_autoresizer_root_volume_size_gib

- Type: Gauge
- Labels: `instance_id`, `device`, `volume_id`, `name`

The most recent root EBS volume size in GiB for one instance. The addon records
it for every discovered instance on each pass (including paused ones, which are
never measured) and updates it immediately after a successful resize.

The size is deliberately a gauge value rather than a label: a label value change
would start a new time series on every resize and break usage history, while a
gauge keeps the series identity stable and shows each resize as a step in the
graph.

The labels are identical to `root_usage_percent`, so the two gauges join
cleanly. In a Grafana table, query both with instant table-format queries and
combine them with a Merge (or Join by field on `instance_id`) transformation to
show usage percent and volume size side by side. In PromQL you can also compute
absolute usage:

```promql
external_ebs_autoresizer_root_volume_size_gib
  * on (instance_id, device, volume_id, name)
external_ebs_autoresizer_root_usage_percent / 100
```

### external_ebs_autoresizer_resize_total

- Type: Counter
- Labels: `result`, `policy`

The total number of resize attempts, split by outcome and the resize policy that
matched the instance. The `result` label is either `success` or `failure`. A
`success` is counted only after the filesystem is fully extended. Any failure
during `modify`, `wait`, or `resize` is counted as `failure`. The `policy` label
is the matched policy name, or `default` for instances matching no named policy.

Use it to track how many resizes happen over time, to catch a rising failure
rate, and to break both down per policy.

### external_ebs_autoresizer_skip_total

- Type: Counter
- Labels: `reason`, `policy`

The total number of instances that the addon looked at but did not resize,
grouped by why it held back and by the matched policy. The `reason` label is one
of:

| Reason | Meaning |
|--------|---------|
| `below_threshold` | Root usage was under the effective `usageThresholdPercent`, so nothing was needed. This is the normal healthy case and grows on every pass. |
| `max_size` | The target size would exceed the effective `maxVolumeSizeGiB`, so the volume was left as is. |
| `cooldown` | The volume was modified within the AWS 6-hour window, or is still modifying, so it could not be grown yet. |
| `dry_run` | `dryRun` is enabled, so the addon only logged what it would have done. |
| `paused` | The matched policy (or `defaultPolicy`) has `paused: true`, so the instance is out of scope and never measured. |

The `policy` label is the matched policy name, or `default`. This metric makes
the addon's silent decisions visible. `resize_total` and `error_total` say
nothing when an instance is above threshold but skipped, so without `skip_total`
a disk can keep filling up at the `max_size` ceiling with no signal at all.
Watch `reason="max_size"` together with `root_usage_percent` to catch volumes
that are stuck and need a manual size bump.

### external_ebs_autoresizer_policy_instances

- Type: Gauge
- Labels: `policy`

The number of discovered instances each resize policy matched in the latest
reconcile pass. The `policy` label is a named policy or `default` (instances
matching no named policy). Every configured policy is reported each pass, set to
`0` when it matches nothing, so a policy whose selector stops matching is
immediately visible.

Use it to confirm a policy's reach after a config change, and to alert when a
policy you expect to cover instances drops to `0`.

### external_ebs_autoresizer_error_total

- Type: Counter
- Labels: `stage`

The total number of errors, grouped by the reconcile stage where each error
happened. The `stage` label is one of `discover`, `measure`, `cooldown`,
`modify`, `wait`, or `resize` (see the Background section for what each stage
does), plus `node_list`, `query_peak`, `query_samples`, `describe_volumes`,
`describe_instance_types`, and `annotate` from the throughput recommender.

This metric is more detailed than `resize_total` because it shows *where* things
break. For example, many errors with `stage="measure"` point to an SSM or
permissions problem, not a volume problem.

### external_ebs_autoresizer_reconcile_total

- Type: Counter
- Labels: none

The total number of reconcile passes that have started. It increases by one each
interval (set by `reconcileInterval`, default `5m`).

Use it as a liveness signal. If this counter stops growing, the reconcile loop
has stalled, even if the Pod still looks healthy.

### external_ebs_autoresizer_node_throughput_current_mibps

- Type: Gauge
- Labels: `node`, `instance_id`, `volume_id`

The provisioned EBS throughput, in MiB/s, of the volume attached to one Kubernetes
Node. Only exported when `throughputRecommendation.enabled` is true.

### external_ebs_autoresizer_node_throughput_observed_peak_mibps

- Type: Gauge
- Labels: `node`, `instance_id`, `volume_id`

The observed peak throughput of one Node over the configured observation window
(the configured quantile of per-step throughput, not the mean). This is the demand
signal the recommendation is derived from.

### external_ebs_autoresizer_node_throughput_recommended_mibps

- Type: Gauge
- Labels: `node`, `instance_id`, `volume_id`

The recommended throughput, in MiB/s. It equals the current value when no change is
recommended, so `recommended > current` is exactly the set of nodes with a pending
increase.

All three gauges carry the same labels on purpose, so headroom is a plain vector
match rather than a relabeling exercise. They are reset at the start of every
recommender pass: nodes are short-lived under Karpenter, and a terminated node's
last reading would otherwise stay exported and read as a live node.

### external_ebs_autoresizer_recommendation_total

- Type: Counter
- Labels: `action`, `reason`

The total number of recommendations published. `action` is one of `increase`,
`decrease`, `none`, or `unknown`; `reason` explains it (for example
`clamped_to_instance_bandwidth`, `insufficient_samples`). The full reason list is in
[designs/ebs-throughput-recommendation.md](designs/ebs-throughput-recommendation.md).

### external_ebs_autoresizer_throughput_apply_total

- Type: Counter
- Labels: `result`

The total number of throughput piggybacks attempted on volume size modifications,
only populated when `throughputRecommendation.applyOnResize` is enabled. An
attempt means a combined size + throughput + IOPS request was sent to EC2;
modifications that proceeded without one are counted separately in
`throughput_apply_skip_total`, the same split `resize_total` and `skip_total` use.

| Result | Meaning |
|--------|---------|
| `applied` | The combined modification succeeded. |
| `fallback_size_only` | The combined request was rejected; the resize was retried (and succeeded or failed on its own merits) without the throughput change. |

### external_ebs_autoresizer_throughput_apply_skip_total

- Type: Counter
- Labels: `reason`

The total number of volume size modifications that proceeded without a throughput
piggyback, by reason. Only counted when a modification actually spends a slot: dry
runs and `applyOnResize: false` configurations count nothing. The label set is
fixed, so the series count does not grow with fleet size.

| Reason | Meaning |
|--------|---------|
| `no_recommendation` | The recommender has never evaluated this volume (standalone instance, multiple attached volumes, node too young). The normal case outside the recommender's scope. |
| `stale` | A recommendation exists but is older than the freshness bound (2 recommender intervals): the recommender has stopped producing while the resizer kept going. The one reason worth alerting on. |
| `not_increase` | A fresh recommendation exists and asks for no raise. The healthy steady state for in-scope volumes. |

### external_ebs_autoresizer_recommender_reconcile_total

- Type: Counter

The total number of throughput recommender passes started, the liveness signal of
the recommender loop the way `reconcile_total` is for the resize loop. Absent when
the recommender is disabled.

### external_ebs_autoresizer_unused_pvc_info

- Type: Gauge (always `1`)
- Labels: `namespace`, `name`, `volume_name`, `volume_id`, `storage_class`,
  `reason`

One series per reported unused PersistentVolumeClaim, carrying every descriptive
label the report is listed by. The value is always `1`: this is an identity
series, not a measurement. `reason` is one of `no_consumer_pod`,
`statefulset_scaled_down`, or `unbound`. `volume_id` is the EBS volume behind the
claim's bound volume, empty when the claim never bound or is not EBS-backed.

Query it alone for the list of unused claims, or join it to the two value series
below for a table that also carries the numbers.

### external_ebs_autoresizer_unused_pv_info

- Type: Gauge (always `1`)
- Labels: `name`, `volume_id`, `storage_class`, `reason`, `reclaim_policy`,
  `claim_namespace`, `claim_name`

One series per reported unused PersistentVolume. `reason` is one of `released`,
`available`, `failed`, `missing_claim`, or `bound_to_unused_claim`.
`reclaim_policy` matters for reading the row: a `Released` volume under `Retain`
is one Kubernetes will never clean up on its own. `claim_namespace` and
`claim_name` are the claim it is or was bound to, empty for a volume that was
never claimed, and they are what lets a table listed by volume name still name
the workload that left it behind.

### external_ebs_autoresizer_unused_pvc_age_seconds

- Type: Gauge
- Labels: `namespace`, `name`

How long a claim has been continuously unused, in seconds. Only claims unused for
at least 24 hours are exported, so a workload between two Pods never appears
here.

### external_ebs_autoresizer_unused_pvc_capacity_bytes

- Type: Gauge
- Labels: `namespace`, `name`

The provisioned capacity of the same claim, keyed identically to the age gauge.

### external_ebs_autoresizer_unused_pv_age_seconds

- Type: Gauge
- Labels: `name`

How long a volume has been continuously unused, in seconds.

### external_ebs_autoresizer_unused_pv_capacity_bytes

- Type: Gauge
- Labels: `name`

The provisioned capacity of the same volume.

#### Why identity and measurement are separate series

The descriptive labels live on the `_info` series and the value gauges are keyed
by nothing but the object's own name. That split is deliberate:

- A series identity that includes `reason` restarts whenever the reason changes,
  so a range query over an object's age would break exactly when something about
  it changed. Keyed by name alone, the age series is continuous for as long as
  the object is unused.
- The label set is written once rather than duplicated across three metrics.

The cost is that a full table needs a join, which is one `group_left` (see the
example queries). The `_info` pattern is the standard Prometheus shape for
exactly this, and Grafana renders the joined result as one table.

### external_ebs_autoresizer_unused_objects

- Type: Gauge
- Labels: `kind`, `reason`

How many objects the latest scan reported, by kind (`persistentvolumeclaim`,
`persistentvolume`) and reason. Every known reason is published on every pass, so
a reason that has stopped occurring reads as `0` rather than vanishing. This is
the series to alert on: the per-object series carry a name label and are too wide
for an alert rule.

### external_ebs_autoresizer_unused_objects_capacity_bytes

- Type: Gauge
- Labels: `kind`, `reason`

The total capacity the reported objects hold, by kind and reason.

### external_ebs_autoresizer_unused_scan_total

- Type: Counter

The total number of unused volume scan passes started. It is incremented before
the pass runs, so it counts attempts rather than outcomes: a pass that cannot
list the cluster still raises it. Use it for "is the loop ticking at all", and
`unused_scan_last_success_timestamp_seconds` for "is the report current".

### external_ebs_autoresizer_unused_scan_failure_total

- Type: Counter

The total number of scan passes that ended in an error. Subtract it from
`unused_scan_total` for the number that succeeded.

This is a different unit of work from `error_total`. A pass fails as a whole when
it cannot read the cluster inventory (`error_total{stage="pv_inventory"}`), while
a per-object annotation failure (`error_total{stage="pv_annotate"}`) is logged,
counted, and skipped without aborting the pass. A pass can therefore raise
`error_total` several times and still succeed.

### external_ebs_autoresizer_unused_scan_last_success_timestamp_seconds

- Type: Gauge

The Unix timestamp of the last scan pass that completed without an error, and `0`
until the first one does.

This is the health signal of the scanner, and the reason it is a timestamp rather
than a boolean `up` gauge. The findings gauges (`unused_objects`,
`unused_pvc_info`, and the rest) are reset and republished on every pass, so they
hold their last values for as long as the process lives. A scanner that stopped
running an hour ago and a cluster with nothing to report look identical in every
one of them. A boolean set at startup has the same defect: it stays `1` while the
loop is wedged. The age of this timestamp is the only thing that separates a
fresh report from a frozen one.

Alert on the age, scoped to the leader:

```promql
(time() - max by (cluster) (
  external_ebs_autoresizer_unused_scan_last_success_timestamp_seconds
    and on (pod) external_ebs_autoresizer_leader == 1
)) > 3 * 3600
```

The scan interval is one hour and is not configurable, so three hours is two
missed passes. Guard against the zero value if you do not want the alert to fire
during the first pass after a restart, which normally lands within seconds of
startup.

### external_ebs_autoresizer_unused_scan_duration_seconds

- Type: Gauge

How long the most recent scan pass took, successful or not. The pass reads the
whole cluster in four list calls, so a rising value is the API server slowing
down before it starts failing outright. It is recorded for failed passes too,
because how long a pass ran before failing separates a slow API server from one
rejecting the calls.

### external_ebs_autoresizer_leader

- Type: Gauge

`1` on the replica currently running the reconcile loops, `0` on every other
replica.

Every loop (resizer, throughput recommender, unused volume scanner) runs under
one leader election, so a follower publishes no scan, resize, or reconcile
activity at all. That is by design, and it is indistinguishable from a leader
whose loops have wedged. Any liveness alert over the counters above must be
scoped with `and on (pod) external_ebs_autoresizer_leader == 1`, or it fires on
every non-leader replica as soon as the Deployment is scaled past one.

When leader election is disabled, or `POD_NAME` is unset, the process runs the
loops directly and reports `1`.

## Example queries

Instances currently above 80% usage:

```promql
external_ebs_autoresizer_root_usage_percent > 80
```

Resize failure rate over the last hour:

```promql
rate(external_ebs_autoresizer_resize_total{result="failure"}[1h])
```

Errors by stage over the last hour:

```promql
sum by (stage) (rate(external_ebs_autoresizer_error_total[1h]))
```

Scan passes that failed over the last day:

```promql
increase(external_ebs_autoresizer_unused_scan_failure_total[1d])
```

How long ago the unused volume report was last refreshed, in seconds:

```promql
time() - max(external_ebs_autoresizer_unused_scan_last_success_timestamp_seconds)
```

Volumes stuck at the max-size ceiling while still filling up (above 90%):

```promql
rate(external_ebs_autoresizer_skip_total{reason="max_size"}[1h]) > 0
  and on() max(external_ebs_autoresizer_root_usage_percent) > 90
```

The full list of unused PersistentVolumes, one row per volume with every label
and both numbers. This is the table query: `group_left` copies the `_info`
labels onto the value series, and the second join adds capacity.

```promql
(
  external_ebs_autoresizer_unused_pv_age_seconds
    * on (name) group_left (volume_id, storage_class, reason, reclaim_policy, claim_namespace, claim_name)
      external_ebs_autoresizer_unused_pv_info
)
```

Capacity instead of age, with the same labels:

```promql
external_ebs_autoresizer_unused_pv_capacity_bytes
  * on (name) group_left (volume_id, storage_class, reason, reclaim_policy, claim_namespace, claim_name)
    external_ebs_autoresizer_unused_pv_info
```

The same list for claims, joined on both identity labels:

```promql
external_ebs_autoresizer_unused_pvc_age_seconds
  * on (namespace, name) group_left (volume_name, volume_id, storage_class, reason)
    external_ebs_autoresizer_unused_pvc_info
```

In Grafana, run either query as a **Table** panel with **Format: Table** and
**Instant** on, then use an *Organize fields* transform to drop `Time` and
`__name__` and to rename `Value`. To show both numbers in one table, add the age
and capacity queries as separate refIds and join them with a *Join by field*
transform on `name`.

Every unused volume of one storage class, released and never reclaimed:

```promql
external_ebs_autoresizer_unused_pv_info{reason="released", reclaim_policy="Retain", storage_class="gp3"}
```

Unused claims left behind by one namespace, whatever the reason:

```promql
external_ebs_autoresizer_unused_pvc_info{namespace="legacy"}
```

Total GiB held by unused claims and volumes, without double counting a claim and
the volume bound to it:

```promql
sum(external_ebs_autoresizer_unused_objects_capacity_bytes{kind="persistentvolumeclaim"}) / 1024^3
  + sum(external_ebs_autoresizer_unused_objects_capacity_bytes{kind="persistentvolume", reason!="bound_to_unused_claim"}) / 1024^3
```

Claims left behind by a StatefulSet scale-down, oldest first. The `and on` filters
the value series by an `_info` selector without copying its labels:

```promql
topk(20,
  external_ebs_autoresizer_unused_pvc_age_seconds
    and on (namespace, name) external_ebs_autoresizer_unused_pvc_info{reason="statefulset_scaled_down"}
)
```

The ten unused claims holding the most storage:

```promql
topk(10, external_ebs_autoresizer_unused_pvc_capacity_bytes)
```

Detect a stalled reconcile loop (no new pass in 15 minutes):

```promql
increase(external_ebs_autoresizer_reconcile_total[15m]) == 0
```

Detect a stopped recommender while resizes keep spending modification slots, from
either side:

```promql
increase(external_ebs_autoresizer_recommender_reconcile_total[2h]) == 0
  or increase(external_ebs_autoresizer_throughput_apply_skip_total{reason="stale"}[6h]) > 0
```

Nodes wanting more EBS throughput than they have:

```promql
external_ebs_autoresizer_node_throughput_recommended_mibps
  > external_ebs_autoresizer_node_throughput_current_mibps
```

Throughput utilization per node, the same number the
`external-ebs-autoresizer/throughput-utilization-percent` annotation carries:

```promql
100 * external_ebs_autoresizer_node_throughput_observed_peak_mibps
  / external_ebs_autoresizer_node_throughput_current_mibps
```

Over 100 is normal rather than a fault: node exporter measures bytes actually moved
and a gp3 volume bursts above its provisioned throughput.

Nodes throughput-bound at a ceiling, where a volume change alone will not help:

```promql
sum by (reason) (
  rate(external_ebs_autoresizer_recommendation_total{reason=~"clamped_to_.*"}[1h])
)
```

## Conclusion

The addon exposes fourteen metrics, and together they answer fourteen simple
questions:

| Question | Metric | Type |
|----------|--------|------|
| How full are the disks? | `root_usage_percent` | Gauge |
| How big are the volumes? | `root_volume_size_gib` | Gauge |
| Are resizes succeeding? | `resize_total` | Counter |
| When the addon holds back, why? | `skip_total` | Counter |
| If something fails, where? | `error_total` | Counter |
| Is the loop still running? | `reconcile_total` | Counter |
| Which policy covers which instances? | `policy_instances` | Gauge |
| What throughput do nodes have? | `node_throughput_current_mibps` | Gauge |
| What throughput do they actually use? | `node_throughput_observed_peak_mibps` | Gauge |
| What should they have? | `node_throughput_recommended_mibps` | Gauge |
| What is being recommended, and why? | `recommendation_total` | Counter |
| Are recommendations being applied on resize? | `throughput_apply_total` | Counter |
| When a spent slot carried no throughput change, why? | `throughput_apply_skip_total` | Counter |
| Is the recommender loop still running? | `recommender_reconcile_total` | Counter |

A good starting point is one dashboard panel per metric, plus three alerts: one
on a rising `resize_total{result="failure"}` rate, one on a stalled
`reconcile_total`, and one on `skip_total{reason="max_size"}` paired with high
`root_usage_percent` to catch disks stuck at the ceiling. From there you can add
per-instance usage views using the labels on `root_usage_percent`.
