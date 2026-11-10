# volume-autoscaler

![Version: 0.2.0](https://img.shields.io/badge/Version-0.2.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.2.0](https://img.shields.io/badge/AppVersion-0.2.0-informational?style=flat-square)

Grows EC2 root volumes and Kubernetes PersistentVolumeClaims when usage crosses a threshold

**Homepage:** <https://github.com/younsl/addons>

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/volume-autoscaler
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `volume-autoscaler`:

```console
helm install volume-autoscaler oci://ghcr.io/younsl/charts/volume-autoscaler
```

Install with custom values:

```console
helm install volume-autoscaler oci://ghcr.io/younsl/charts/volume-autoscaler -f values.yaml
```

Install a specific version:

```console
helm install volume-autoscaler oci://ghcr.io/younsl/charts/volume-autoscaler --version 0.2.0
```

### Install from local chart

Download volume-autoscaler chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/volume-autoscaler --untar --version 0.2.0
helm install volume-autoscaler ./volume-autoscaler
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade volume-autoscaler oci://ghcr.io/younsl/charts/volume-autoscaler
```

## Uninstall

```console
helm uninstall volume-autoscaler
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | `""` | Override the chart name |
| fullnameOverride | string | `""` | Override the fully qualified release name |
| image.registry | string | `"ghcr.io"` | Container image registry host |
| image.repository | string | `"younsl/volume-autoscaler"` | Container image repository path without registry prefix |
| image.tag | string | `""` | Image tag, defaults to appVersion |
| image.digest | string | `""` | Image digest (`sha256:...`), takes precedence over tag |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries |
| replicaCount | int | `1` | Replicas. Above 1 enables leader election |
| revisionHistoryLimit | int | `3` | Number of old ReplicaSets to retain for rollback |
| strategy | object | `{"rollingUpdate":{"maxSurge":"25%","maxUnavailable":"25%"},"type":"RollingUpdate"}` | Deployment update strategy |
| strategy.rollingUpdate.maxSurge | string|int | `"25%"` | Max surge during an update |
| strategy.rollingUpdate.maxUnavailable | string|int | `"25%"` | Max unavailable during an update |
| pdb.enabled | bool | `true` | Create a PodDisruptionBudget |
| pdb.maxUnavailable | int|string | `1` | Max unavailable Pods |
| pdb.unhealthyPodEvictionPolicy | string | `"IfHealthyBudget"` | `IfHealthyBudget` or `AlwaysAllow` |
| controllers | list | `["instance-volume-autoscaler","persistent-volume-claim-autoscaler","unused-volume-scan"]` | Controllers to run (`--controllers`). RBAC follows the enabled ones |
| crds.install | bool | `true` | Install and upgrade both CRDs |
| crds.keep | bool | `true` | Keep the CRDs on `helm uninstall` |
| crds.annotations | object | `{}` | Extra CRD annotations |
| crds.additionalLabels | object | `{}` | Extra CRD labels |
| instanceVolumeAutoscalers | list | `[]` | InstanceVolumeAutoscalers to render (`name`, `labels`, `annotations`, `spec`). See examples/ |
| persistentVolumeClaimAutoscalers | list | `[]` | PersistentVolumeClaimAutoscalers to render, like the above plus `namespace` |
| config.region | string | `""` | AWS region (required) |
| config.reconcileInterval | string | `"5m"` | InstanceVolumeAutoscaler reconcile interval |
| config.claimReconcileInterval | string | `"1m"` | PersistentVolumeClaimAutoscaler reconcile interval, at least 10s |
| config.reconcileConcurrency | int | `10` | Max instances reconciled in parallel |
| config.ssmCommandTimeout | string | `"5m"` | SSM command timeout |
| config.ssmPollInterval | string | `"1s"` | SSM and volume modification poll interval |
| config.volumeModifyTimeout | string | `"10m"` | ModifyVolume optimizing-wait timeout |
| config.dryRun | bool | `false` | Measure and decide only, never modify |
| config.logLevel | string | `"info"` | `debug`, `info`, `warn`, or `error` |
| config.logFormat | string | `"json"` | `json` or `text` |
| config.alertmanager.enabled | bool | `false` | Alert resize outcomes to Alertmanager, requires url |
| config.alertmanager.url | string | `""` | Alertmanager v2 base URL |
| config.alertmanager.timeout | string | `"5s"` | Alertmanager POST timeout |
| config.alertmanager.labels | object | `{}` | Labels added to every alert |
| config.alertmanager.notifyOn | string | `"success"` | Outcomes to alert: `all`, `success`, or `failure` |
| config.alertmanager.dashboardUrl | string | `""` | Dashboard link template with {instance_id} and similar, empty disables |
| config.grafanaAnnotation.enabled | bool | `false` | Annotate resize outcomes in Grafana, requires url and a token |
| config.grafanaAnnotation.url | string | `"http://grafana.monitoring:3000"` | Grafana base URL |
| config.grafanaAnnotation.timeout | string | `"5s"` | Grafana POST timeout |
| config.grafanaAnnotation.tags | list | `["event:ebs-resize"]` | Tags added to every annotation |
| config.grafanaAnnotation.annotateOn | string | `"all"` | Outcomes to annotate: `all`, `success`, or `failure` |
| config.grafanaAnnotation.apiToken | string | `""` | Grafana token, stored in a generated Secret. Prefer existingSecret |
| config.grafanaAnnotation.existingSecret | string | `""` | Existing Secret holding the Grafana token |
| config.grafanaAnnotation.existingSecretKey | string | `"token"` | Token key in the Secret |
| config.metrics.url | string | `""` | Prometheus or Mimir base URL. Token auth via `PROMETHEUS_BEARER_TOKEN` in extraEnv |
| config.metrics.tenantId | string | `""` | Mimir tenant (`X-Scope-OrgID`), empty for Prometheus |
| config.metrics.labelSelector | object | `{}` | Label matchers added to every query |
| config.throughputRecommendation | object | `{"enabled":false,"interval":"30m","lookbackWindow":"7d","metricNodeNameLabel":"node"}` | Node gp3 throughput recommendations as Node annotations |
| config.throughputRecommendation.enabled | bool | `false` | Enable recommendations, requires metrics.url |
| config.throughputRecommendation.metricNodeNameLabel | string | `"node"` | Metric label holding the Node name (`node`, or `instance` for a plain node exporter) |
| config.throughputRecommendation.lookbackWindow | string | `"7d"` | Observation window as a Prometheus duration (7d, 12h) |
| config.throughputRecommendation.interval | string | `"30m"` | Recommendation interval |
| config.orphanPvcCleanup | object | `{"alertBefore":"6h","alertEnabled":true,"enabled":false,"excludeNamespaces":["kube-system"],"includeNamespaces":[],"maxDeletionsPerPass":1,"minUnusedAge":"3d","reasons":["no_consumer_pod","statefulset_scaled_down","unbound"]}` | Orphan PVC cleanup: deletes PersistentVolumeClaims the unused volume scan reports, never a PersistentVolume or EBS volume |
| config.orphanPvcCleanup.enabled | bool | `false` | Enable deletion, requires the `unused-volume-scan` controller |
| config.orphanPvcCleanup.minUnusedAge | string | `"3d"` | How long a claim stays unused before it is deleted, at least 24h. Units `m`, `h`, `d` |
| config.orphanPvcCleanup.reasons | list | `["no_consumer_pod","statefulset_scaled_down","unbound"]` | Unused reasons eligible for deletion: `no_consumer_pod`, `statefulset_scaled_down`, `unbound` |
| config.orphanPvcCleanup.includeNamespaces | list | `[]` | Namespaces in scope, empty means all |
| config.orphanPvcCleanup.excludeNamespaces | list | `["kube-system"]` | Namespaces never cleaned up |
| config.orphanPvcCleanup.maxDeletionsPerPass | int | `1` | Max claims deleted per hourly scan, 1 to 100 |
| config.orphanPvcCleanup.alertEnabled | bool | `true` | Alert upcoming, deleted, and failed deletions to Alertmanager, requires alertmanager.enabled |
| config.orphanPvcCleanup.alertBefore | string | `"6h"` | Schedule the claim and send the one upcoming alert this long before deletion, at least 1h |
| extraEnv | list | `[]` | Extra container env (EnvVar entries) |
| extraEnvFrom | list | `[]` | Extra container envFrom sources |
| ports.health | int | `8080` | Health port (/healthz, /readyz) |
| ports.metrics | int | `8081` | Metrics port (/metrics) |
| rbac.create | bool | `true` | Create RBAC resources |
| serviceAccount.create | bool | `true` | Create a ServiceAccount |
| serviceAccount.name | string | `""` | ServiceAccount name, defaults to the fullname |
| serviceAccount.annotations | object | `{}` | ServiceAccount annotations, e.g. an IRSA role ARN |
| serviceAccount.automountServiceAccountToken | bool | `true` | Automount the ServiceAccount token |
| serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets on the ServiceAccount |
| service.enabled | bool | `true` | Create a Service for health and metrics |
| service.type | string | `"ClusterIP"` | Service type |
| service.trafficDistribution | string | `""` | Traffic distribution, e.g. PreferClose, empty omits |
| serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor |
| serviceMonitor.interval | string | `"30s"` | Scrape interval |
| serviceMonitor.scrapeTimeout | string | `"10s"` | Scrape timeout |
| serviceMonitor.labels | object | `{}` | Extra ServiceMonitor labels |
| resources | object | `{"limits":{"memory":"128Mi"},"requests":{"cpu":"25m","memory":"64Mi"}}` | Pod resources |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"NotRequired"}]` | Container resize policy, empty omits |
| podAnnotations | object | `{}` | Extra Pod annotations |
| podLabels | object | `{}` | Extra Pod labels |
| podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod-level security context |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container-level security context |
| priorityClassName | string | `""` | PriorityClass name, empty omits |
| nodeSelector | object | `{}` | Node selector |
| tolerations | list | `[]` | Tolerations |
| affinity | object | `{}` | Affinity |
| topologySpreadConstraints | list | `[]` | Topology spread constraints, labelSelector defaults to the Pod selector labels |
| dnsPolicy | string | `""` | Pod DNS policy, empty omits |
| dnsConfig | object | `{}` | Pod DNS config, empty omits |
| extraObjects | list | `[]` | Extra manifests rendered verbatim |

## Source Code

* <https://github.com/younsl/addons/tree/main/box/addons/volume-autoscaler>

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| younsl | <cysl@kakao.com> | <https://github.com/younsl> |

## License

This chart is licensed under the Apache License 2.0. See the [LICENSE](https://github.com/younsl/addons/blob/main/LICENSE) file for details.

## Contributing

This repository does not accept external contributions. Pull requests and issues are disabled.

----------------------------------------------
Autogenerated from chart metadata using [helm-docs v1.14.2](https://github.com/norwoodj/helm-docs/releases/v1.14.2)
