# Metrics

## Overview

This document describes the Prometheus metrics that ec2-metadata-exporter
exposes, what each metric means, and how to use them for dashboards and
alerts.

The exporter publishes metrics on the `/metrics` HTTP path. The default port
is `8081` and can be changed with the `METRICS_PORT` environment variable.
All metric names share the prefix `ec2_metadata_`. Output uses the
OpenMetrics text format (`application/openmetrics-text`), which Prometheus
scrapes natively.

## Metric reference

| Metric | Type | Description |
|--------|------|-------------|
| `ec2_metadata_instance_info{instance_id, name, private_ip, private_dns_name, instance_type, availability_zone, state, lifecycle, architecture}` | Gauge | Always 1. One series per non-terminated instance with a private IP. `lifecycle` is `on-demand` or `spot`, `architecture` is `x86_64`, `arm64`, etc. `private_dns_name` is also the Kubernetes node name on EKS clusters using the default IP-based naming, so it joins directly onto `kube_node_info`. |
| `ec2_metadata_instance_launch_time_seconds{instance_id, name}` | Gauge | Unix timestamp of the instance's most recent launch. Resets on stop/start, so `time()` minus this value is uptime since the last boot, not since creation. Omitted when EC2 returns no launch time. |
| `ec2_metadata_instance_metadata_options{instance_id, name, http_tokens, http_endpoint, hop_limit, imdsv1_allowed}` | Gauge | Always 1. IMDS configuration. EC2 exposes no version field, so `imdsv1_allowed` is derived: it is `true` only when the endpoint is `enabled` and `http_tokens` is `optional`, meaning both IMDS versions answer. `required` tokens are IMDSv2-only and a `disabled` endpoint answers neither version. `hop_limit` below 2 stops containers from reaching IMDS at all. Omitted when EC2 returns no metadata options block. |
| `ec2_metadata_instances{state}` | Gauge | Instance count from the last successful scrape, broken down by instance state. Sum over `state` for the total. |
| `ec2_metadata_scrape_errors_total` | Counter | EC2 API scrape failures. |
| `ec2_metadata_scrape_duration_seconds` | Histogram | EC2 API scrape duration. Buckets from 50ms to ~25.6s. |
| `ec2_metadata_last_scrape_success_timestamp_seconds` | Gauge | Unix time of the last successful scrape. |
| `ec2_metadata_build_info{version, commit, rust_version}` | Gauge | Always 1. Exporter version, git commit, and Rust compiler version. |

Example output:

```
ec2_metadata_instance_info{instance_id="i-0abc123",name="web-1",private_ip="10.0.1.10",private_dns_name="ip-10-0-1-10.ap-northeast-2.compute.internal",instance_type="m5.large",availability_zone="ap-northeast-2a",state="running",lifecycle="on-demand",architecture="x86_64"} 1
ec2_metadata_instance_launch_time_seconds{instance_id="i-0abc123",name="web-1"} 1.752994800e+09
ec2_metadata_instance_metadata_options{instance_id="i-0abc123",name="web-1",http_tokens="required",http_endpoint="enabled",hop_limit="2",imdsv1_allowed="false"} 1
ec2_metadata_instances{state="running"} 1
ec2_metadata_build_info{version="0.2.0",commit="0e44eb2",rust_version="1.98.0"} 1
```

Instance metrics are served from an in-memory snapshot that is swapped
atomically on every successful refresh: a Prometheus scrape never observes a
half-populated result, and terminated instances drop out as soon as a new
snapshot lands. When a refresh fails, the previous snapshot keeps serving and
`ec2_metadata_last_scrape_success_timestamp_seconds` stops advancing.

## Example queries

| Purpose | PromQL |
|---------|--------|
| Resolve instance name by private IP | `ec2_metadata_instance_info{private_ip="10.0.1.10"}` |
| Running instances per type | `count by (instance_type) (ec2_metadata_instance_info{state="running"})` |
| Spot ratio | `count(ec2_metadata_instance_info{lifecycle="spot"}) / count(ec2_metadata_instance_info)` |
| Total instances across states | `sum(ec2_metadata_instances)` |
| Stopped instance count | `ec2_metadata_instances{state="stopped"}` |
| Instance uptime (seconds since last boot) | `time() - ec2_metadata_instance_launch_time_seconds` |
| Instances up longer than 90 days | `count((time() - ec2_metadata_instance_launch_time_seconds) > 90 * 86400)` |
| Restart detection (stop/start in the last hour) | `changes(ec2_metadata_instance_launch_time_seconds[1h]) > 0` |
| Scrape error rate | `rate(ec2_metadata_scrape_errors_total[5m])` |
| Scrape latency p99 | `histogram_quantile(0.99, rate(ec2_metadata_scrape_duration_seconds_bucket[5m]))` |
| Staleness (seconds since last success) | `time() - ec2_metadata_last_scrape_success_timestamp_seconds` |
| Deployed exporter versions | `count by (version, rust_version) (ec2_metadata_build_info)` |
| Kubernetes node to EC2 join | `kube_node_info * on (node) group_left (instance_type, lifecycle) label_replace(ec2_metadata_instance_info, "node", "$1", "private_dns_name", "(.*)")` |
| Instances still answering IMDSv1 | `ec2_metadata_instance_metadata_options{imdsv1_allowed="true"}` |
| IMDSv2 enforcement ratio | `count(ec2_metadata_instance_metadata_options{imdsv1_allowed="false"}) / count(ec2_metadata_instance_metadata_options)` |
| Instances whose hop limit blocks pod IMDS access | `ec2_metadata_instance_metadata_options{hop_limit="1"}` |

## Alerting hints

- Alert when `time() - ec2_metadata_last_scrape_success_timestamp_seconds`
  exceeds several scrape intervals; the info labels are stale beyond that
  point.
- Alert on a sustained increase of `ec2_metadata_scrape_errors_total`, which
  usually indicates IAM or EC2 API throttling problems.
- A rising `ec2_metadata_scrape_duration_seconds` p99 signals EC2 API
  throttling or a growing instance fleet before errors start appearing.
- Alert on any `ec2_metadata_instance_metadata_options{imdsv1_allowed="true"}`
  series to catch instances that never enforced IMDSv2.

## Cardinality

`ec2_metadata_instance_info`, `ec2_metadata_instance_launch_time_seconds` and
`ec2_metadata_instance_metadata_options` each produce one series per instance,
so the exporter's series count is roughly three times the fleet size. Adding a
label to one of those metrics costs bytes per scrape but no extra series.

EC2 tags other than `Name` are not collected. On Kubernetes nodes the same
information already arrives through `kube_node_labels`, and tag compliance
belongs to AWS Config or the Resource Groups Tagging API rather than to a
Prometheus exporter.

## Readiness behavior

The `/readyz` endpoint on the health port stays not-ready until the first
successful EC2 scrape completes, so rollouts never route to an exporter with
an empty snapshot. After that it stays ready; scrape failures keep serving
the previous snapshot and are surfaced through
`ec2_metadata_scrape_errors_total` and the last-success timestamp instead of
flipping readiness.
