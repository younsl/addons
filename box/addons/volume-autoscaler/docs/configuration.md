# Configuration

Every config file key and its default. Read this when writing the chart values or the mounted config file. Which instances and claims are resized, and how, is declared in [InstanceVolumeAutoscaler](instance-volume-autoscaler.md) and [PersistentVolumeClaimAutoscaler](persistent-volume-claim-autoscaler.md) resources rather than here. Which controllers run is chosen with `--controllers` (env `CONTROLLERS`), see [CLI](cli.md).

All settings are read from a single YAML config file, mounted from a [ConfigMap][k8s-configmap]
at `/etc/volume-autoscaler/config.yaml` (override the path with
`CONFIG_FILE`). The Helm chart renders this file from `.Values.config`. Any key
omitted from the file takes its default. Parsing is strict: an unknown key fails
at startup. Two values are injected from the environment instead of the file:
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
```

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

## Resize settings

The usage target, step size, ceiling, pause switch, and per-group alert switch live in [InstanceVolumeAutoscaler](instance-volume-autoscaler.md) resources. The retired `defaultPolicy`, `policies`, `tagFilters`, and `excludeEKSNodes` keys fail at startup with a pointer to them. The controller discovers every running instance in its region, and only instances an autoscaler's `scaleTargetRef` targets are resized.

[k8s-configmap]: https://kubernetes.io/docs/concepts/configuration/configmap/
[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-secret]: https://kubernetes.io/docs/concepts/configuration/secret/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-events]: https://kubernetes.io/docs/reference/kubernetes-api/cluster-resources/event-v1/

## Conclusion

Every setting comes from one strictly parsed YAML file, so an unknown key fails at startup and an omitted key takes its default, while the Pod identity, `GRAFANA_API_TOKEN`, and `PROMETHEUS_BEARER_TOKEN` come from the environment so no credential lands in the ConfigMap. Resize settings are InstanceVolumeAutoscaler resources, not config keys.
