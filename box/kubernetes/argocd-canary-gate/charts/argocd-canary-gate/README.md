# argocd-canary-gate

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

Admission gate that blocks an Argo CD Application sync while an Argo Rollouts canary is still in progress

**Homepage:** <https://github.com/younsl/o>

## Requirements

Kubernetes: `>=1.30.0-0`

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/argocd-canary-gate
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `argocd-canary-gate`:

```console
helm install argocd-canary-gate oci://ghcr.io/younsl/charts/argocd-canary-gate
```

Install with custom values:

```console
helm install argocd-canary-gate oci://ghcr.io/younsl/charts/argocd-canary-gate -f values.yaml
```

Install a specific version:

```console
helm install argocd-canary-gate oci://ghcr.io/younsl/charts/argocd-canary-gate --version 0.1.0
```

### Install from local chart

Download argocd-canary-gate chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/argocd-canary-gate --untar --version 0.1.0
helm install argocd-canary-gate ./argocd-canary-gate
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade argocd-canary-gate oci://ghcr.io/younsl/charts/argocd-canary-gate
```

## Uninstall

```console
helm uninstall argocd-canary-gate
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| replicaCount | int | `1` | Number of gate replicas. Keep above one while `webhook.failurePolicy` is `Fail` |
| image.registry | string | `"ghcr.io"` | Container image registry host |
| image.repository | string | `"younsl/argocd-canary-gate"` | Container image repository path without registry prefix |
| image.tag | string | `""` | Image tag; defaults to the chart appVersion when empty |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries |
| nameOverride | string | `""` | Overrides the chart name used in resource names |
| fullnameOverride | string | `""` | Overrides the full generated resource name |
| serviceAccount.create | bool | `true` | Create a ServiceAccount for the gate |
| serviceAccount.annotations | object | `{}` | Annotations added to the ServiceAccount |
| serviceAccount.name | string | `""` | Existing ServiceAccount name when `create` is false |
| rbac.create | bool | `true` | Create the ClusterRole for reading Rollouts and the Role for writing Events |
| podAnnotations | object | `{}` | Annotations added to the gate pods |
| podLabels | object | `{}` | Labels added to the gate pods |
| podSecurityContext | object | `{"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod-level security context |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container-level security context |
| resources | object | `{"limits":{"memory":"128Mi"},"requests":{"cpu":"20m","memory":"64Mi"}}` | Resource requests and limits. No CPU limit: throttling admission turns latency into failed syncs |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | Container resize policy for in-place vertical scaling. A cpu resize applies without a restart, a memory resize needs one |
| nodeSelector | object | `{}` | Node selector for the gate pods |
| tolerations | list | `[]` | Tolerations for the gate pods |
| topologySpreadConstraints | list | `[{"labelSelector":{"matchLabels":{"app.kubernetes.io/name":"argocd-canary-gate"}},"maxSkew":1,"topologyKey":"kubernetes.io/hostname","whenUnsatisfiable":"ScheduleAnyway"}]` | Topology spread constraints. Spreading replicas keeps one node loss from failing every gated sync |
| affinity | object | `{}` | Affinity rules for the gate pods |
| podDisruptionBudget.enabled | bool | `true` | Create a PodDisruptionBudget |
| podDisruptionBudget.minAvailable | int/string | `1` | Minimum available replicas during voluntary disruption |
| podDisruptionBudget.unhealthyPodEvictionPolicy | string | `"IfHealthyBudget"` | `IfHealthyBudget` guards a healthy replica while the budget is met. `AlwaysAllow` drops that guard. Empty leaves it unset |
| service.type | string | `"ClusterIP"` | Service type |
| service.trafficDistribution | string | `""` | Endpoint routing preference. `PreferSameZone` or `PreferSameNode`, or the deprecated `PreferClose`. Empty leaves it unset |
| service.webhookPort | int | `443` | Port the API server calls for admission review |
| service.adminPort | int | `8080` | Port serving probes and metrics |
| serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor. Skipped when the cluster has no Prometheus Operator CRD |
| serviceMonitor.namespace | string | `""` | Namespace to create the ServiceMonitor in. Empty uses the release namespace |
| serviceMonitor.selector | object | `{}` | Labels the Prometheus `serviceMonitorSelector` matches on |
| serviceMonitor.additionalLabels | object | `{}` | Extra labels added to the ServiceMonitor |
| serviceMonitor.annotations | object | `{}` | Annotations added to the ServiceMonitor |
| serviceMonitor.interval | string | `"30s"` | Scrape interval. Empty leaves the Prometheus default |
| serviceMonitor.scrapeTimeout | string | `"10s"` | Scrape timeout. Empty leaves the Prometheus default |
| serviceMonitor.honorLabels | bool | `false` | Keep labels the target exposes when they collide with server-side labels |
| serviceMonitor.scheme | string | `""` | Scheme for the scrape. Empty leaves the Prometheus default |
| serviceMonitor.tlsConfig | object | `{}` | TLS settings for the scrape |
| serviceMonitor.relabelings | list | `[]` | Relabeling applied to the discovered target |
| serviceMonitor.metricRelabelings | list | `[]` | Relabeling applied to each scraped metric |
| logging.level | string | `"info"` | Log level: debug, info, warn, error |
| logging.format | string | `"json"` | Log format: json or text |
| webhook.failurePolicy | string | `"Fail"` | `Fail` keeps the gate from being bypassed by an outage. `Ignore` trades enforcement for availability |
| webhook.timeoutSeconds | int | `5` | Admission timeout. The gate answers with one Rollout list, so a few seconds is plenty |
| webhook.certValidityDays | int | `3650` | Serving certificate lifetime in days. The Secret is reused across upgrades |
| webhook.extraMatchConditions | list | `[]` | Extra CEL match conditions appended to the generated ones |
| webhook.certManager.enabled | bool | `false` | Hand the serving certificate to cert-manager instead of minting one with `genCA`, which a renderer without cluster access re-mints on every pass |
| webhook.certManager.issuerRef | object | `{}` | Issue from an existing issuer such as `{kind: ClusterIssuer, name: internal-ca}`. Empty makes the chart create its own self-signed and CA Issuer |
| uiExtension.name | string | `"canary-gate"` | Name argocd-server proxies under `/extensions/<name>/...`. Must match argocd-cm |
| canaryGate.mode | string | `"enforce"` | `enforce` denies a sync while a watched Rollout has an update in flight. `warn` lets it through with a warning |
| canaryGate.onError | string | `"deny"` | Verdict when the Rollout list itself fails: `allow` or `deny` |
| canaryGate.argocd.namespace | string | `"argocd"` | Namespace holding the Application resources. Denial Events are written here |
| canaryGate.rollouts.trackingLabel | string | `"argocd.argoproj.io/instance"` | Label tying a Rollout to its Application. Argo CD stamps its tracking label on every managed resource with the Application name as the value |
| canaryGate.rollouts.strategies | list | `["canary"]` | Rollout strategies the gate watches: `canary`, `blueGreen` |
| canaryGate.exempt.usernames | list | `["system:serviceaccount:argocd:argocd-application-controller"]` | Principals whose sync requests bypass the gate. Keep the application controller listed |
| canaryGate.exempt.automated | bool | `true` | Bypass the gate when Argo CD marks the operation automated |
| canaryGate.exempt.annotation | string | `"canary-gate.younsl.github.io/skip"` | Application annotation that opts one app out when set to `"true"` |
| extraObjects | list | `[]` | Extra manifests rendered as-is and templated with the release context |

## Source Code

* <https://github.com/younsl/o/tree/main/box/kubernetes/argocd-canary-gate>

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| younsl | <cysl@kakao.com> | <https://github.com/younsl> |

## License

This chart is licensed under the Apache License 2.0. See the [LICENSE](https://github.com/younsl/o/blob/main/LICENSE) file for details.

## Contributing

This repository does not accept external contributions. Pull requests and issues are disabled.

----------------------------------------------
Autogenerated from chart metadata using [helm-docs v1.14.2](https://github.com/norwoodj/helm-docs/releases/v1.14.2)
