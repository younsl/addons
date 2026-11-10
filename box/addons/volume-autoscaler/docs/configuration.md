# Configuration

Every config file key and its default. Read this when writing the chart values or the mounted config file. Which instances and claims are resized, and how, is declared in [InstanceVolumeAutoscaler](instance-volume-autoscaler.md) and [PersistentVolumeClaimAutoscaler](persistent-volume-claim-autoscaler.md) resources rather than here. Which controllers run is chosen with `--controllers` (env `CONTROLLERS`), see [CLI](cli.md).

All settings are read from a single YAML config file, mounted from a [ConfigMap][k8s-configmap]
at `/etc/volume-autoscaler/config.yaml` (override the path with
`CONFIG_FILE`). The Helm chart renders this file from `.Values.config`. Any key
omitted from the file takes its default. Invalid values fail open, see
[Invalid values](#invalid-values). Two values are injected from the environment instead of the file:
the [Pod][k8s-pod] identity (`POD_NAME` / `POD_NAMESPACE` / `POD_UID`, via the downward
API) and `GRAFANA_API_TOKEN` (from a [Secret][k8s-secret]), so the token never lands in a
ConfigMap.

```yaml
region: ap-northeast-2                 # required
reconcileInterval: 5m                  # duration: 30s, 5m, 1h, 1h30m
claimReconcileInterval: 1m             # PersistentVolumeClaimAutoscaler re-evaluation, at least 10s
reconcileConcurrency: 10               # max instances reconciled in parallel per pass
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
metrics:                               # shared by PersistentVolumeClaimAutoscaler and throughput recommendations
  url: ""                              # Prometheus, or a Mimir query-frontend/gateway
  tenantId: ""                         # X-Scope-OrgID; empty for Prometheus
  labelSelector: {}                    # matchers added to every query, e.g. cluster: prd on a central Mimir
throughputRecommendation:              # node gp3 throughput recommendations; see throughput-recommendation.md
  enabled: false                       # requires metrics.url when true
  metricNodeNameLabel: node                      # metric label carrying the Node name; "instance" for a plain node exporter scrape
  lookbackWindow: 7d                   # a Prometheus duration (7d, 12h), unlike the other intervals
  interval: 30m                        # separate from reconcileInterval
  applyOnResize: true                  # piggyback an increase onto a size expansion; false keeps recommendations advisory-only
orphanPvcCleanup:                      # deletes unused claims, never a volume; see orphan-pvc-cleanup.md
  enabled: false                       # requires the unused-volume-scan controller
  minUnusedAge: 7d                     # deleted once unused this long, at least 24h; the chart sets 3d
  reasons:
    - no_consumer_pod
    - statefulset_scaled_down
    - unbound
  includeNamespaces: []                # empty means every namespace
  excludeNamespaces: [kube-system]     # wins over includeNamespaces
  maxDeletionsPerPass: 1               # 1 to 100 per hourly scan
  alertEnabled: true                   # alert upcoming, deleted, failed; requires alertmanager.enabled
  alertBefore: 6h                      # schedule and alert this long before deletion; at least 1h
```

Every key exists since app version 0.1.0, except `orphanPvcCleanup`, which was added in 0.2.0.

A third environment-injected value applies whenever `metrics.url` is queried:
`PROMETHEUS_BEARER_TOKEN`, for a gateway that fronts the metrics backend with token
auth. Like the Grafana token it is never a config-file key, so it stays out of the
ConfigMap; inject it from a Secret via the chart's `extraEnv`.

`LOG_LEVEL` and `LOG_FORMAT`, when set, override `logLevel` and `logFormat`. `RUST_LOG` overrides the level with a full filter directive.

Everything the recommender does not list above (the observation quantile, headroom,
recommendation step, throughput bounds, device matcher, query timeout, [annotation][k8s-annotations]
prefix) is fixed policy in the addon rather than a setting. See
[designs/ebs-throughput-recommendation.md](designs/ebs-throughput-recommendation.md#what-is-no-longer-configurable)
for each value and why.

## Durations

Every duration key takes whole numbers with a unit: `s`, `m`, `h`, or `d` (24 hours), combined in descending order such as `1d12h` or `2h30m`. `ms`, `us`, and `ns` also parse. Fractions such as `1.5h` or `1.5d` are invalid. `throughputRecommendation.lookbackWindow` is the exception: it is a Prometheus duration, interpolated into PromQL, so it follows the Prometheus grammar (`7d`, `12h`).

## Invalid values

An invalid value never keeps the controller down. It is replaced, logged at WARN once at startup (`configuration value corrected, the controller starts anyway`), and printed by `volume-autoscaler validate` as a `warning:` line:

| Problem | What happens |
|---------|--------------|
| Unparseable value (`5min`, `1.5d`, an unknown `notifyOn`) | The key takes its default |
| Out of range (`minUnusedAge: 12h`, `maxDeletionsPerPass: 0`) | Clamped to the nearest valid value |
| A feature missing what it needs (`alertmanager.enabled` without `url`) | That feature is turned off |
| Unknown key or retired key | Ignored |
| Unknown `orphanPvcCleanup.reasons` entry | Dropped. An empty result falls back to every reason |

Only three problems still fail startup, because no default can stand in for them: a missing `region`, malformed YAML, and a value of the wrong type (`dryRun: maybe`). The Helm chart's `values.schema.json` still rejects invalid values at install time, so a chart install catches them before they reach the controller.

## Resize settings

The usage target, step size, ceiling, pause switch, and per-group alert switch live in [InstanceVolumeAutoscaler](instance-volume-autoscaler.md) resources. The retired `defaultPolicy`, `policies`, `tagFilters`, and `excludeEKSNodes` keys fail at startup with a pointer to them. The controller discovers every running instance in its region, and only instances an autoscaler's `scaleTargetRef` targets are resized.

[k8s-configmap]: https://kubernetes.io/docs/concepts/configuration/configmap/
[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-secret]: https://kubernetes.io/docs/concepts/configuration/secret/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-events]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/

## Conclusion

Every setting comes from one YAML file where an omitted key takes its default and an invalid value is corrected and logged rather than failing startup, while the Pod identity, `GRAFANA_API_TOKEN`, and `PROMETHEUS_BEARER_TOKEN` come from the environment so no credential lands in the ConfigMap. Resize settings are InstanceVolumeAutoscaler resources, not config keys.
