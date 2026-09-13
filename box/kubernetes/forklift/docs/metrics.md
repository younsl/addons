# Metrics

## Overview

This document explains the
[Prometheus](https://github.com/prometheus/prometheus) metrics that forklift
exposes. It describes what each metric means, which labels it carries, and how
to use it to build dashboards and alerts.

Read this if you run forklift and want to watch it in production, or if you are
on call and need to check whether downloads, caching, approvals, or HA
replication are healthy. Basic familiarity with Prometheus and PromQL is enough.

## Background

forklift runs as a single Rust process. The application (API, UI, package
endpoints) serves on `FORKLIFT_HTTP_ADDR` (`:8080` by default). Metrics are
served separately on `FORKLIFT_METRICS_ADDR` (`:8081` by default) at the
`/metrics` path, so a scrape never competes with package traffic.

All forklift metric names share the prefix `forklift_`. The endpoint also
exposes the standard process collector (`process_*`), which is not documented
here.

Three kinds of metrics need a note on how they are computed:

- The inventory and storage gauges (`forklift_repositories`,
  `forklift_artifacts`, `forklift_blobs`, `forklift_storage_bytes`) and
  `forklift_approval_pending` are computed at scrape time by querying the
  metadata store. They carry no leader gating, so they stay accurate on standby
  pods after a replication snapshot swap.
- `forklift_upstream_up` is refreshed by a background prober that checks every
  proxy repository's upstream once a minute. It runs on every pod, so in HA
  each pod reports its own network view of the upstreams.
- The traffic, cache, policy, and replication metrics are counters, gauges, and
  histograms updated as requests flow through the process.

A short reminder on metric types:

- A **gauge** is a value that can go up and down. It always reports the latest
  reading.
- A **counter** only goes up and resets to zero on process restart. Look at how
  fast it grows with `rate()`, not at its raw value.
- A **histogram** records observations into buckets, used here for request
  latency.

## Metrics

### forklift_build_info

- Type: Gauge
- Labels: `version`, `commit`, `rust_version`

Build metadata exposed as a constant gauge whose value is always `1`. The labels
carry the running binary version, the Git commit it was built from, and the Rust
toolchain version. Use it to confirm which build is live and to join other
metrics against a version during a rollout.

### forklift_leader

- Type: Gauge
- Labels: none

`1` if this instance currently holds leadership, otherwise `0`. In a
single-instance deployment the process is always leader and reports `1`. In HA
mode exactly one pod reports `1` at a time, decided by
[Kubernetes](https://github.com/kubernetes/kubernetes) Lease leader election.
The leader runs the gated background work (blob sweeper, audit retention) and,
with PV-based replication, serves snapshots to standbys.

Use it to confirm that exactly one leader exists. A sum across pods that is not
`1` points to a split brain or a stalled election.

### forklift_leader_transitions_total

- Type: Counter
- Labels: none

The number of times this instance acquired leadership since it started. A
single-instance deployment records exactly one transition at startup. In HA
mode, sum the rate across pods: leadership should change hands only on a
deliberate failover or a pod restart, so a rising sum with no rollout in
progress means the Lease is flapping. Because replication is asynchronous,
every unplanned transition can lose the writes of the last sync interval, which
makes flapping worth an alert of its own even when `forklift_leader` always
sums to `1`.

### forklift_http_requests_total

- Type: Counter
- Labels: `method`, `route`, `status`

Total HTTP requests served by the application listener, split by HTTP method,
matched route pattern, and response status code. Use it for request rate and
error-ratio dashboards.

### forklift_http_request_duration_seconds

- Type: Histogram
- Labels: `method`, `route`, `status`

Request latency in seconds, bucketed with the Prometheus default buckets. Use
the `_bucket` series with `histogram_quantile()` for latency percentiles, and
the `_count` series as an alternative request counter.

### forklift_db_connections_open / forklift_db_connections_in_use / forklift_db_connections_idle / forklift_db_connections_max

- Type: Gauge
- Labels: `pool` (`write`, `read`)

Connection-pool state for the metadata database. The `write` pool is deliberately
capped at one connection (SQLite tolerates a single writer), so
`forklift_db_connections_in_use{pool="write"}` sitting at 1 is normal; the
interesting signal is how long callers wait for it, below.

### forklift_db_connection_waits_total / forklift_db_connection_wait_seconds_total

- Type: Counter
- Labels: `pool` (`write`, `read`)

How often, and for how long in total, a caller had to wait for a free
connection. This is the saturation signal for the metadata store: because every
write shares one connection, an overloaded instance does not show up as CPU or
query volume but as waiting. Sustained growth on `pool="write"` means writes are
queueing, which is what makes the console, package traffic and the readiness
probe slow at the same time.

### forklift_readyz_failures_total

- Type: Counter
- Labels: `reason` (`not_leader`, `database`)

Readiness refusals by reason. A pod dropped from the Service looks identical from
outside whichever way it happened, so this separates a standby (or a step-down)
from an instance that could not reach the database within
`readyzTimeout`.

### forklift_readyz_duration_seconds

- Type: Histogram
- Labels: none

Readiness probe handler latency, database check included. The probe's own
timeout is short (one second by default in the chart), so rising percentiles here
are the early warning that the database is congested, before
`forklift_readyz_failures_total{reason="database"}` starts moving and the pod
leaves the Service.

### forklift_scan_ratio_lookups_total / forklift_scan_ratio_refresh_seconds

- Type: Counter (`result` = `hit`, `miss`) / Histogram
- Labels: `result` / none

The repository list's scan-coverage aggregate. Computing it reads every
versioned artifact and the whole scan table, so it is cached and recomputed at
most once per window: `result="miss"` counts the recomputations and the histogram
their cost. A miss rate that tracks request rate means the cache is not holding,
and a growing refresh duration is inventory growth showing up in that scan.

### forklift_coverage_percent / forklift_coverage_target / forklift_coverage_projects

- Type: Gauge
- Labels: none / none / `state`

How much of the organisation's source builds through this forklift, as measured
by the GitLab scan (see [Coverage](coverage.md)). `target` is the denominator:
in-scope projects that have CI. `percent` is applied over target.

| `state` | Meaning |
|---------|---------|
| `applied` | Every check still being asked for is present, which is both halves unless one was muted |
| `partial` | Only one of the two does, with neither muted |
| `not_applied` | Has CI, references forklift nowhere |
| `error` | The scan could not read the project |
| `no_ci` | No GitLab CI at all, so out of the denominator |
| `muted` | Every check silenced from the console, or a GitLab topic |

A `target` of zero is worth alerting on by itself: it almost always means the
access token can no longer see the projects, not that they disappeared.

### forklift_coverage_last_scan_timestamp_seconds / forklift_coverage_last_scan_duration_seconds

- Type: Gauge
- Labels: none

When the last scan completed, and how long it took. The timestamp is zero before
the first scan.

This is the metric to alert on. A scan that stops running is invisible from
outside: the console keeps showing the last result, so the number simply stops
moving and nobody notices. Alert on the age of the timestamp rather than on the
coverage number:

```promql
time() - forklift_coverage_last_scan_timestamp_seconds > 172800
  and forklift_coverage_enabled == 1
```

### forklift_coverage_scans_total

- Type: Counter
- Labels: `result` (`success`, `failure`)

Scan attempts since process start. Counted per process, so a restart resets
them; the timestamp above is what survives.

A rising `failure` count alongside a stale timestamp separates the two ways this
breaks: the schedule is firing but the crawl is failing, which is the access
token or the GitLab instance, not the cron expression.

### forklift_coverage_enabled / forklift_coverage_scanning

- Type: Gauge
- Labels: none

`enabled` is 1 when coverage scanning is switched on and has a GitLab
connection. It is exported even when off, so a dashboard can say "off" instead
of showing a gap that reads as broken, and so an age alert can exclude the
deployments that never turned it on. `scanning` is 1 while a crawl is in flight.

### forklift_coverage_gitlab_concurrency / forklift_coverage_gitlab_concurrency_peak

- Type: Gauge
- Labels: none

The in-flight request limit the adaptive controller settled on during the last
crawl, and the highest it reached. The crawl has no rate setting: it raises the
limit while GitLab answers promptly and cuts it on a 429, a 5xx, or a latency
spike. These two are therefore the only way to see what rate the instance turned
out to tolerate, and a limit that collapses to 1 between scans is the instance
pushing back.

### forklift_repositories

- Type: Gauge
- Labels: `format`, `type`

The number of configured repositories, grouped by package family and
repository type.

| Label | Values |
|-------|--------|
| `format` | `maven`, `npm`, `cargo`, `go`, `pypi`, `raw`, `oci` |
| `type` | `hosted`, `proxy`, `group` |

Use it to track repository inventory and to confirm that expected repositories
exist after a config change.

### forklift_artifacts

- Type: Gauge
- Labels: none

The total number of logical artifacts indexed across all repositories. This is a
metadata count, not physical storage. Two repositories that reference the same
content count as two artifacts even though the bytes are stored once.

### forklift_blobs

- Type: Gauge
- Labels: none

The number of deduplicated content-addressed blobs in the blob store. Each blob
is unique by SHA-256, so this is always less than or equal to
`forklift_artifacts`. The ratio between the two shows how effective
deduplication is.

### forklift_storage_bytes

- Type: Gauge
- Labels: none

Physical bytes used by the deduplicated blobs on the PersistentVolume. Use it to
watch storage growth and to size or alert on the volume.

### forklift_blobstore_operation_duration_seconds

- Type: Histogram
- Labels: `backend`, `op`, `result`

Latency of blob store backend operations. The `backend` label is `fs` for the
filesystem store or `s3` for the S3 store, `op` is one of `put`, `open`,
`open_seekable`, `exists`, or `delete`, and `result` is `success`,
`not_found` (an expected missing blob, not a failure), or `error`.

`put` includes streaming the whole source body (an upstream response or a
client upload), so its duration reflects transfer size as much as backend
speed; the read paths (`open`, `exists`) are the cleaner backend health signal.
This metric matters most with the S3 backend, where a bucket outage, throttling
or IAM regression would otherwise be invisible until downloads start failing.
On `fs` it surfaces a degraded volume (for example an exhausted burst balance).

### forklift_bytes_transferred_total

- Type: Counter
- Labels: `direction`, `format`

Artifact bytes transferred between forklift and its clients. The `direction`
label is `egress` for downloads served to clients and `ingress` for uploads
received from clients. The `format` label is the package family. Use it for
bandwidth dashboards and per-format traffic breakdowns.

### forklift_cache_hits_total / forklift_cache_misses_total

- Type: Counter
- Labels: `repo`

Proxy cache outcomes per repository. A hit means a proxy repository served an
artifact from its local cache; a miss means it had to fetch from upstream. The
hit ratio per repo measures cache effectiveness.

### forklift_upstream_errors_total

- Type: Counter
- Labels: `repo`

Failures while fetching from an upstream, per proxy repository. A rising rate
points to a broken or unreachable upstream, not a forklift problem. Pair it with
`forklift_cache_misses_total` to see how many misses turned into errors.

### forklift_upstream_request_duration_seconds

- Type: Histogram
- Labels: `repo`

Latency of upstream fetches per proxy repository, measured from sending the
request until the response headers arrive (or the transport fails). The buckets
extend to the 60 second upstream client timeout, so a slow-but-alive upstream
shows up in the tail long before requests start failing. Errors and timeouts
are observed too, which is why the error rate and this histogram move together
when an upstream degrades. Use it to tell "upstream is slow" from "upstream is
down": builds that hang usually show tail latency growth here first.

### forklift_upstream_up

- Type: Gauge
- Labels: `repo`

`1` if the proxy repository's upstream answered the most recent background
probe with any HTTP response (even a 4xx), `0` if the probe failed at the
transport level (DNS, connect, TLS, timeout). Probes run once a minute on every
pod with a 5 second timeout, mirroring the reachability semantics of the
console's upstream health check. Unlike `forklift_upstream_errors_total`, which
only moves when clients request packages, this gauge detects a dead upstream
even on an idle repository. Repositories deleted from the configuration drop
out of the gauge on the next probe cycle.

### forklift_metadata_rewrite_wait_seconds

- Type: Histogram
- Labels: `repo`

Time a metadata request spent queued for a rewrite slot. Packument and simple
index documents are decoded under a small semaphore (see
`forklift_metadata_rewrite_capacity`) because decoding one costs several times
its size in heap, so an install burst that resolves hundreds of packages at once
queues here rather than multiplying that cost by the request count.

The buckets reach 60 seconds because that is where installers give up: both npm
and pnpm default to a 60 second fetch timeout, and their timeout is armed when
the request is created, not when it reaches a connection. A queue this deep is
therefore invisible in the response metrics, since the client disconnects before
forklift writes a status code. Waits that ended that way are observed here too,
and counted in `forklift_metadata_rewrite_abandoned_total`.

### forklift_metadata_rewrite_duration_seconds

- Type: Histogram
- Labels: `repo`

Time a metadata request held a rewrite slot, covering the decode and the URL
rewrite. Read it against the wait histogram to tell the two saturation causes
apart: a long wait with a short hold means the gate is too narrow for the
offered concurrency, while a long hold means each decode is slow, which in
practice means the process is short of CPU (compare with
`forklift_upstream_request_duration_seconds`, which stays flat in that case
because the upstream is not involved).

### forklift_metadata_rewrite_abandoned_total

- Type: Counter
- Labels: `repo`

Metadata requests whose client disconnected while still queued for a rewrite
slot. This is the direct signal that metadata serving is saturated: the requests
it counts produce no response and no status code, so they appear nowhere in
`forklift_http_requests_total`. Any sustained rate here means installers are
timing out.

### forklift_metadata_render_cache_total

- Type: Counter
- Labels: `result` (`hit`, `miss`)

Lookups against the rendered-document cache. A hit serves a packument or simple
index that was already decoded and rewritten for the same stored bytes, external
base URL, serving repository and repository configuration, which skips both the
decode and the rewrite gate entirely. Rendered documents live for 30 seconds, so
the hit ratio reflects repeat requests for the same package: a second CI job
resolving the same lockfile, or an installer retrying after a timeout. A retry
storm with a low hit ratio here means retries are costing as much as the burst
that triggered them.

### forklift_metadata_rewrite_inflight / forklift_metadata_rewrite_queued / forklift_metadata_rewrite_capacity

- Type: Gauge
- Labels: none

Rewrite slots currently held, requests currently waiting for one, and the total
number of slots. `inflight / capacity` is gate saturation; a non-zero `queued`
that persists across scrapes means requests are being served slower than they
arrive.

### forklift_age_policy_violations_total

- Type: Counter
- Labels: `repo`, `action`

Requests that hit the supply-chain age policy, which quarantines freshly
published upstream versions. The `action` label distinguishes a hard block from
a warning. Use it to gauge how often the age policy intervenes.

### forklift_approval_blocked_total

- Type: Counter
- Labels: `repo`, `mode`

Requests handled by the package approval gate per repository. The `mode` label
is `enforce` when the request was blocked pending an admin decision, or `audit`
when it was only counted (audit-only mode lets the request through). Use it to
size the approval workload before switching a repo into enforce mode.

### forklift_approval_pending

- Type: Gauge
- Labels: none

Package approval requests currently waiting for an admin decision. Computed at
scrape time. Alert on a value that stays high, which means the approval queue is
not being worked.

### forklift_version_deny_blocked_total

- Type: Counter
- Labels: `repo`

Requests blocked by the per-version deny list, which blocks one exact package
version (for example a poisoned release or a known IOC) while the package itself
stays approved. A spike after adding a deny entry confirms clients are still
trying to pull the bad version.

### forklift_oci_prune_deleted_total

- Type: Counter
- Labels: `repo`

OCI artifact rows deleted by the reachability prune: untagged manifests and the
blobs no live manifest references. The OCI format is exempt from the idle
retention reaper and LRU cache eviction (either would break still-tagged
images), so this counter is the format's only automated space reclamation. The
freed bytes are reclaimed by the ordinary blob sweeper after its grace period.

### forklift_oci_upload_sessions_active

- Type: Gauge

OCI blob push sessions currently in flight: opened with a POST but not yet
finalized, cancelled or expired. Counted from the sessions table, so the value
is correct across replicas; each scrape publishes the count taken at the
previous scrape, so the series trails by one scrape interval. A value that only
grows means clients are opening pushes and never completing them; the prune
expires such sessions after `FORKLIFT_OCI_UPLOAD_SESSION_TTL`. Reports -1 on
the first scrape after start and when the count query fails.

### forklift_vuln_blocked_total

- Type: Counter
- Labels: `repo`, `action`

Requests counted by the vulnerability policy: blocked when `action=block`, or
recorded-only when `action=warn`/`audit`. The policy matches the requested
package version against [OSV](https://github.com/google/osv.dev) advisories
(direct dependency only) and triggers when the highest non-ignored severity
meets the configured threshold. A spike indicates clients pulling versions with
known advisories.

### forklift_vuln_scans_total

- Type: Counter
- Labels: `result` (`clean`, `vulnerable`, `error`)

Vulnerability scans performed by the background worker against OSV. `error`
growth means OSV lookups are failing (unreachable endpoint, rate limit), in
which case unscanned coordinates fail open unless `block_unscanned` is set.

### forklift_license_blocked_total

- Type: Counter
- Labels: `repo`, `action`

Requests counted by the license policy: blocked when `action=block`, or
recorded-only when `action=warn`/`audit`. The policy matches the requested
package version's resolved SPDX license(s) against the per-repository `deny` and
`allow` lists (direct coordinate only). A spike indicates clients pulling
versions whose license is denied (or outside a non-empty allow list). See
[License scanning](license-scanning.md).

### forklift_license_resolves_total

- Type: Counter
- Labels: `result` (`resolved`, `unknown`, `error`)

License resolutions performed by the background worker against
[deps.dev](https://github.com/google/deps.dev). `resolved` means at least one
SPDX license was returned, `unknown` means the source reported none (the
coordinate is still recorded so it is not re-queried every request), and `error`
growth means deps.dev lookups are failing (unreachable endpoint, rate limit), in
which case unresolved coordinates fail open unless `block_unresolved` is set.

### forklift_audit_events_dropped_total

- Type: Counter
- Labels: none

Audit events dropped because the recorder's write buffer was full. This should
stay flat at zero. Any growth means audit writes cannot keep up with traffic and
the audit log is incomplete, so alert on `increase() > 0`.

### Replication metrics

These metrics appear only when PV-based replication is enabled
(`replication.enabled`). They are emitted by standby pods that pull from the
leader.

#### forklift_replication_syncs_total

- Type: Counter
- Labels: `result`

Replication sync cycles by outcome (`result` is `success` or `failure`). A
rising failure rate means a standby is falling behind the leader.

#### forklift_replication_blobs_fetched_total

- Type: Counter
- Labels: none

Blobs downloaded from the leader since startup.

#### forklift_replication_blobs_deleted_total

- Type: Counter
- Labels: none

Local blobs deleted because the leader no longer has them, keeping the standby's
blob store in step with the leader.

#### forklift_replication_last_sync_timestamp_seconds

- Type: Gauge
- Labels: none

Unix time of the last successful sync cycle. Alert on `time() - metric` growing
past a few sync intervals, which means replication has stalled.

#### forklift_replication_snapshot_bytes

- Type: Gauge
- Labels: none

Size of the last database snapshot downloaded from the leader.

### S3 metadata snapshot metrics

The `forklift_objstore_meta_*` counters and gauges appear only with the S3
backend (`storage.backend=s3`). They track the leader's periodic
[SQLite](https://github.com/sqlite/sqlite) snapshot upload and the standby's
snapshot download, labelled by `result`, and are the S3-mode equivalent of the
replication metrics above: a stalled snapshot means the failover data-loss
window is growing past `FORKLIFT_STORAGE_META_SYNC_INTERVAL`.

`result="unchanged"` counts cycles that transferred nothing: the leader found no
commit since its last upload, or the standby found the object's ETag unchanged.
Those cycles still advance `forklift_objstore_meta_last_sync_timestamp_seconds`,
because S3 already holds the current state. A steady `unchanged` rate on the
leader is the idle signature; a leader that only ever reports `ok` is writing
every interval, and the snapshot cadence, not the change detection, bounds its
upload traffic.

## Profiling

Metrics say how much CPU the process burns; only a profile says which code
burns it. The binary serves a `pprof`-compatible CPU profile on a separate
loopback listener (`FORKLIFT_PPROF_ADDR`, `127.0.0.1:6060` by default) rather
than on the metrics port, because the metrics port is published through the
Service and the profiling endpoints have no authentication. `kubectl
port-forward` dials the pod's loopback, so the listener is reachable exactly
that way and no other:

```sh
kubectl -n forklift port-forward pod/<leader-pod> 6060:6060
curl -s 'http://localhost:6060/debug/pprof/profile?seconds=30' -o cpu.pb.gz
go tool pprof -top cpu.pb.gz
```

`/debug/pprof/` lists the endpoints and `/debug/pprof/cmdline` returns the
command line. For memory diagnostics, watch `container_memory_rss` and `process_resident_memory_bytes`
for memory instead. Take the profile while CPU is elevated; a profile from a
quiet pod only shows the baseline.

## Example queries

Confirm exactly one leader across all pods:

```promql
sum(forklift_leader)
```

HTTP error ratio over the last 5 minutes:

```promql
sum(rate(forklift_http_requests_total{status=~"5.."}[5m]))
  / sum(rate(forklift_http_requests_total[5m]))
```

Request latency p99 by route:

```promql
histogram_quantile(0.99,
  sum by (le, route) (rate(forklift_http_request_duration_seconds_bucket[5m])))
```

Metadata write pool saturation (seconds spent waiting per second of wall clock;
approaching 1 means writes are continuously queued):

```promql
rate(forklift_db_connection_wait_seconds_total{pool="write"}[5m])
```

Readiness failures by reason, to tell a step-down from a congested database:

```promql
sum by (reason) (rate(forklift_readyz_failures_total[5m]))
```

Proxy cache hit ratio per repository:

```promql
sum by (repo) (rate(forklift_cache_hits_total[1h]))
  / (sum by (repo) (rate(forklift_cache_hits_total[1h]))
     + sum by (repo) (rate(forklift_cache_misses_total[1h])))
```

Rendered-document reuse (a hit skips the decode and the gate):

```promql
sum(rate(forklift_metadata_render_cache_total{result="hit"}[5m]))
  / sum(rate(forklift_metadata_render_cache_total[5m]))
```

Metadata rewrite gate saturation and queue depth:

```promql
forklift_metadata_rewrite_inflight / forklift_metadata_rewrite_capacity
forklift_metadata_rewrite_queued
```

Installers giving up while queued for a rewrite slot (these requests never reach
`forklift_http_requests_total`):

```promql
sum by (repo) (rate(forklift_metadata_rewrite_abandoned_total[5m]))
```

Queue wait against decode time, to separate a narrow gate from a slow decode:

```promql
histogram_quantile(0.99,
  sum by (le, repo) (rate(forklift_metadata_rewrite_wait_seconds_bucket[5m])))
histogram_quantile(0.99,
  sum by (le, repo) (rate(forklift_metadata_rewrite_duration_seconds_bucket[5m])))
```

Deduplication ratio (artifacts per stored blob):

```promql
forklift_artifacts / forklift_blobs
```

Egress bandwidth by package format:

```promql
sum by (format) (rate(forklift_bytes_transferred_total{direction="egress"}[5m]))
```

Proxy upstream unreachable (fires even on an idle repository):

```promql
forklift_upstream_up == 0
```

Upstream latency p99 per repository (a slow upstream before it turns into
errors):

```promql
histogram_quantile(0.99,
  sum by (le, repo) (rate(forklift_upstream_request_duration_seconds_bucket[5m])))
```

Blob store backend error ratio (S3 outage, throttling, or IAM regression):

```promql
sum(rate(forklift_blobstore_operation_duration_seconds_count{result="error"}[5m]))
  / sum(rate(forklift_blobstore_operation_duration_seconds_count[5m]))
```

Leadership flapping (more than one acquisition in 15 minutes with no rollout):

```promql
sum(increase(forklift_leader_transitions_total[15m])) > 1
```

Audit events being dropped (should never fire):

```promql
increase(forklift_audit_events_dropped_total[15m]) > 0
```

Replication stalled (no successful sync in 10 minutes):

```promql
time() - forklift_replication_last_sync_timestamp_seconds > 600
```

## Conclusion

The metrics answer a few core questions about a forklift deployment:

- Which build is running, and who is leader? `build_info`, `leader`,
  `leader_transitions_total`
- Is traffic healthy? `http_requests_total`, `http_request_duration_seconds`,
  `bytes_transferred_total`
- How big is the repository, and how well does dedup work? `repositories`,
  `artifacts`, `blobs`, `storage_bytes`
- Is the blob store backend healthy? `blobstore_operation_duration_seconds`
- Are proxies caching and reaching upstreams? `cache_*`,
  `upstream_errors_total`, `upstream_request_duration_seconds`, `upstream_up`
- Are the supply-chain gates doing their job? `age_policy_violations_total`,
  `approval_blocked_total`, `approval_pending`, `version_deny_blocked_total`,
  `vuln_blocked_total`, `vuln_scans_total`, `license_blocked_total`,
  `license_resolves_total`
- Is auditing complete? `audit_events_dropped_total`
- In HA, are standbys keeping up? `replication_*`

A good starting point is one dashboard row per group above, plus alerts on
`sum(forklift_leader) != 1`, leadership flapping via
`leader_transitions_total`, any `forklift_upstream_up == 0`, a rising HTTP 5xx
ratio, a rising blob store error ratio, any `audit_events_dropped_total`
growth, and a stalled `replication_last_sync_timestamp_seconds`.
