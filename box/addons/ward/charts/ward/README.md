# ward

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

Kubernetes-native open source antivirus and EDR operator with eBPF node agents

**Homepage:** <https://github.com/younsl/addons>

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/ward
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `ward`:

```console
helm install ward oci://ghcr.io/younsl/charts/ward
```

Install with custom values:

```console
helm install ward oci://ghcr.io/younsl/charts/ward -f values.yaml
```

Install a specific version:

```console
helm install ward oci://ghcr.io/younsl/charts/ward --version 0.1.0
```

### Install from local chart

Download ward chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/ward --untar --version 0.1.0
helm install ward ./ward
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade ward oci://ghcr.io/younsl/charts/ward
```

## Uninstall

```console
helm uninstall ward
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | `""` | Override the chart name |
| fullnameOverride | string | `""` | Override the fully qualified release name |
| image.registry | string | `"ghcr.io"` | Container image registry host |
| image.repository | string | `"younsl/ward"` | Container image repository path without registry prefix |
| image.tag | string | `""` | Image tag, defaults to appVersion |
| image.digest | string | `""` | Image digest (`sha256:...`), takes precedence over tag |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy, also passed to the workloads the operator creates |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries |
| replicaCount | int | `1` | Operator replicas. Above 1 enables Lease leader election and a PodDisruptionBudget |
| revisionHistoryLimit | int | `3` | Number of old ReplicaSets to retain for rollback |
| crds.install | bool | `true` | Install and upgrade the Ward and WardSignature CRDs |
| crds.keep | bool | `true` | Keep the CRDs on `helm uninstall` |
| crds.annotations | object | `{}` | Extra CRD annotations |
| crds.additionalLabels | object | `{}` | Extra CRD labels |
| operator.allowedImagePrefixes | list | `[]` | Image prefixes a Ward's `spec.image` may use, empty allows only this chart's image repository |
| operator.allowPrivilegedAgent | bool | `false` | Honor `spec.agent.privileged`, otherwise such a Ward is refused |
| operator.postgresqlImage | string | `"docker.io/library/postgres:18.6-alpine3.24"` | Image of the embedded PostgreSQL server a Ward deploys with `spec.postgresql`. A Ward may override it only within `allowedImagePrefixes` |
| operator.logLevel | string | `"info"` | `debug`, `info`, `warn`, or `error` |
| operator.logFormat | string | `"json"` | `json` or `text` |
| ward.create | bool | `false` | Render a Ward object from `ward.spec` |
| ward.name | string | `"ward"` | Ward object name |
| ward.labels | object | `{}` | Extra Ward object labels |
| ward.annotations | object | `{}` | Extra Ward object annotations |
| ward.spec | object | `{}` | Ward spec, see examples/ward.yaml |
| extraEnv | list | `[]` | Extra container env (EnvVar entries) |
| extraEnvFrom | list | `[]` | Extra container envFrom sources |
| ports.http | int | `8081` | Health and metrics port (/healthz, /readyz, /metrics) |
| rbac.create | bool | `true` | Create RBAC resources |
| serviceAccount.create | bool | `true` | Create a ServiceAccount |
| serviceAccount.name | string | `""` | ServiceAccount name, defaults to the fullname |
| serviceAccount.annotations | object | `{}` | ServiceAccount annotations |
| serviceAccount.automountServiceAccountToken | bool | `true` | Automount the ServiceAccount token |
| serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets on the ServiceAccount |
| service.enabled | bool | `true` | Create a Service for health and metrics |
| service.type | string | `"ClusterIP"` | Service type |
| service.trafficDistribution | string | `""` | Traffic distribution, e.g. PreferClose, empty omits |
| podMonitor.enabled | bool | `false` | Create a PodMonitor for the manager and agent Pods of every Ward |
| podMonitor.interval | string | `"30s"` | Scrape interval |
| podMonitor.scrapeTimeout | string | `"10s"` | Scrape timeout |
| podMonitor.labels | object | `{}` | Extra PodMonitor labels |
| wardNamespacePolicy.enabled | bool | `true` | Deny Ward objects outside the release namespace, needs ValidatingAdmissionPolicy (1.30+) |
| leaseAdmissionPolicy.enabled | bool | `true` | Deny an agent writing any Lease but `ward-<own node>`, needs ValidatingAdmissionPolicy (1.30+) |
| serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor for the operator |
| serviceMonitor.interval | string | `"30s"` | Scrape interval |
| serviceMonitor.scrapeTimeout | string | `"10s"` | Scrape timeout |
| serviceMonitor.labels | object | `{}` | Extra ServiceMonitor labels |
| resources | object | `{"limits":{"memory":"128Mi"},"requests":{"cpu":"25m","memory":"64Mi"}}` | Operator Pod resources |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"NotRequired"}]` | Container resize policy, empty omits |
| podAnnotations | object | `{}` | Extra Pod annotations |
| podLabels | object | `{}` | Extra Pod labels |
| podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod-level security context |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container-level security context |
| priorityClassName | string | `""` | PriorityClass name, empty omits |
| nodeSelector | object | `{}` | Node selector |
| tolerations | list | `[]` | Tolerations |
| affinity | object | `{}` | Affinity |
| extraObjects | list | `[]` | Extra manifests rendered verbatim |

## Source Code

* <https://github.com/younsl/addons/tree/main/box/addons/ward>

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
