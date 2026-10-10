# Configuration

Every config file key, its default, and how per-group resize policies override them. Read this when writing the chart values or the mounted config file.

All settings are read from a single YAML config file, mounted from a [ConfigMap][k8s-configmap]
at `/etc/external-ebs-autoresizer/config.yaml` (override the path with
`CONFIG_FILE`). The Helm chart renders this file from `.Values.config`. Any key
omitted from the file takes its default. Parsing is strict: an unknown key fails
at startup. Two values are injected from the environment instead of the file:
the [Pod][k8s-pod] identity (`POD_NAME` / `POD_NAMESPACE` / `POD_UID`, via the downward
API) and `GRAFANA_API_TOKEN` (from a [Secret][k8s-secret]), so the token never lands in a
ConfigMap.

```yaml
region: ap-northeast-2                 # required
tagFilters: ""                         # "Key=Value,Key2=Value2"; empty scans all instances in the account/region
excludeEKSNodes: true                  # drop EKS nodes (managed node groups, self-managed, Karpenter)
reconcileInterval: 5m                  # duration: 30s, 5m, 1h, 1h30m
reconcileConcurrency: 10               # max instances reconciled in parallel per pass
defaultPolicy:                         # volume-expansion settings for instances matching no named policy (see Per-group resize policies)
  usageThresholdPercent: 80            # REQUIRED. usage that triggers a resize
  growMode: percent                    # REQUIRED. percent (by growPercent) or absolute (by growAmount)
  paused: false                        # true stops the resizer from touching those instances
  alertEnabled: true                   # false mutes Alertmanager alerts for those instances (needs alertmanager.enabled)
  autoProtectiveCordon: false          # true cordons the instance's Node while usage is at or above the threshold (see protective-auto-cordon.md)
  growPercent: 10                      # growth percent per resize (growMode: percent)
  growAmount: 10GiB                    # absolute growth with a MiB/GiB unit (growMode: absolute); MiB rounds up to whole GiB
  maxVolumeSizeGiB: 1000               # safety ceiling
ssmCommandTimeout: 5m
ssmPollInterval: 1s                    # delay between SSM command and volume modification status polls
volumeModifyTimeout: 10m               # ModifyVolume optimizing-wait timeout
dryRun: false                          # measure and decide only
leaderElect: true                      # HA leader election; requires in-cluster config
logLevel: info                         # debug, info, warn, error
logFormat: json                        # json or text
alertmanager:
  enabled: false                       # requires url when true
  url: ""                              # Alertmanager v2 base URL, e.g. http://alertmanager:9093
  timeout: 5s
  labels: {}                           # static Key: Value labels merged into every alert for routing
  notifyOn: success                    # all, success, or failure
  dashboardUrl: ""                     # optional Slack dashboard link template; {instance_id}, {volume_id}, {device}, {instance_name}
grafanaAnnotation:
  enabled: false                       # requires url and GRAFANA_API_TOKEN when true
  url: http://grafana.monitoring:3000
  timeout: 5s
  tags: [event:ebs-resize]             # base tags merged into every annotation
  annotateOn: all                      # all, success, or failure
throughputRecommendation:              # node gp3 throughput recommendations; see throughput-recommendation.md
  enabled: false                       # requires prometheusUrl when true
  prometheusUrl: ""                    # Prometheus, or a Mimir query-frontend/gateway
  prometheusTenantId: ""               # X-Scope-OrgID; empty for Prometheus
  metricNodeNameLabel: node                      # metric label carrying the Node name; "instance" for a plain node exporter scrape
  lookbackWindow: 7d                   # a Prometheus duration (7d, 12h), unlike the other intervals
  interval: 30m                        # separate from reconcileInterval
  applyOnResize: true                  # piggyback an increase onto a size expansion; false keeps recommendations advisory-only
policies: []                           # per-instance-group overrides; see below
```

A third environment-injected value applies when the recommender is enabled:
`PROMETHEUS_BEARER_TOKEN`, for a gateway that fronts the metrics backend with token
auth. Like the Grafana token it is never a config-file key, so it stays out of the
ConfigMap; inject it from a Secret via the chart's `extraEnv`.

`LOG_LEVEL` and `LOG_FORMAT`, when set, override `logLevel` and `logFormat`. `RUST_LOG` overrides the level with a full filter directive.

Everything the recommender does not list above (the observation quantile, headroom,
recommendation step, throughput bounds, device matcher, query timeout, [annotation][k8s-annotations]
prefix) is fixed policy in the addon rather than a setting. See
[designs/ebs-throughput-recommendation.md](designs/ebs-throughput-recommendation.md#what-is-no-longer-configurable)
for each value and why.

## Per-group resize policies

By default every managed instance uses `defaultPolicy`. To vary the resize
behavior per group of instances, add entries to `policies`. Each policy selects
a group via `instanceSelector` and overrides a subset of the resize settings for
that group under its own `resize` block:

```yaml
policies:
  - name: db-nodes
    weight: 10                         # highest weight wins when multiple policies match one instance
    instanceSelector:
      tags:                            # every tag must match exactly
        Role: database
      nameRegex: "^prod-db-.*"         # RE2 regex on the Name tag; tags and nameRegex are ANDed
    resize:
      usageThresholdPercent: 70
      growMode: absolute
      growAmount: 50GiB
      maxVolumeSizeGiB: 2000
  - name: batch-workers
    weight: 1
    instanceSelector:
      nameRegex: "^batch-.*"
    resize:
      paused: true                     # stop resizing this group without deleting its config
      alertEnabled: false              # mute alerts for this group without touching the others
      growPercent: 30
```

Matching rules: `instanceSelector` needs at least one of `tags` (exact
equality on every listed key) or `nameRegex` (unanchored RE2 against the Name
tag); when both are set they are ANDed. Among all matching policies the highest
`weight` wins, ties fall back to list order (earliest wins), and an instance
matching no policy uses `defaultPolicy`. Any `resize` field a policy omits is
inherited from `defaultPolicy`. The matched policy name is attached to each
instance's logs (`policy=<name>`, or `policy=default`).

`defaultPolicy` and a policy's `resize` block share the same fields, but differ
in what is required: `defaultPolicy.usageThresholdPercent` and
`defaultPolicy.growMode` must be declared (startup fails otherwise) since they
are the baseline for every unmatched instance, while every field in a policy's
`resize` block is optional and inherits from `defaultPolicy` when omitted.

Set `paused: true` on a policy (or on `defaultPolicy`) to take its instances out
of scope: they are skipped without being measured or resized (skip reason
`paused`). This is a config-only kill switch for a group, leaving the rest of
its settings intact for when you resume.

Set `alertEnabled: false` on a policy (or on `defaultPolicy`) to mute
Alertmanager alerts for its instances while the rest keep alerting. The global
`alertmanager.enabled` switch remains the master gate; per-policy `alertEnabled`
only subtracts from it and defaults to true. It affects alerts only: metrics,
Kubernetes [Events][k8s-events], and Grafana annotations are still recorded.

`tagFilters` still scopes which instances are discovered at all (a server-side
EC2 filter); policies only tune the resize parameters of already-discovered
instances.

[k8s-configmap]: https://kubernetes.io/docs/concepts/configuration/configmap/
[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-secret]: https://kubernetes.io/docs/concepts/configuration/secret/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-events]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/
