# node-configuration-operator

![Version: 0.2.0](https://img.shields.io/badge/Version-0.2.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.2.0](https://img.shields.io/badge/AppVersion-0.2.0-informational?style=flat-square)

Continuously reconciles the OS configuration of EKS managed node group nodes from NodeConfiguration custom resources

**Homepage:** <https://github.com/younsl/addons>

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/node-configuration-operator
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `node-configuration-operator`:

```console
helm install node-configuration-operator oci://ghcr.io/younsl/charts/node-configuration-operator
```

Install with custom values:

```console
helm install node-configuration-operator oci://ghcr.io/younsl/charts/node-configuration-operator -f values.yaml
```

Install a specific version:

```console
helm install node-configuration-operator oci://ghcr.io/younsl/charts/node-configuration-operator --version 0.2.0
```

### Install from local chart

Download node-configuration-operator chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/node-configuration-operator --untar --version 0.2.0
helm install node-configuration-operator ./node-configuration-operator
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade node-configuration-operator oci://ghcr.io/younsl/charts/node-configuration-operator
```

## Uninstall

```console
helm uninstall node-configuration-operator
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | `""` | Overrides the chart name used in resource names and the app.kubernetes.io/name label |
| fullnameOverride | string | `""` | Overrides the full generated resource name |
| crds.install | bool | `true` | Whether to install CRDs. Set to false if CRDs are managed externally. |
| crds.keep | bool | `true` | Whether to keep CRDs on chart uninstall. |
| crds.annotations | object | `{}` | Annotations to add to the CRD resources. |
| crds.additionalLabels | object | `{}` | Additional labels to add to the CRD resources. |
| replicaCount | int | `1` | Number of controller replicas to run. Above 1 requires `leaderElection.enabled`, and the replicas beyond the leader stand by. |
| leaderElection.enabled | bool | `true` | Whether a controller replica acts only while it holds a Lease in the release namespace. One replica keeps it on so that two pods never act at once during a rolling update. |
| leaderElection.leaseDurationSeconds | int | `15` | Seconds a standby waits on a Lease that is not renewed before it takes over. |
| leaderElection.renewDeadlineSeconds | int | `10` | Seconds the leader keeps trying to renew the Lease before it steps down. Must be below `leaseDurationSeconds`. |
| leaderElection.retryPeriodSeconds | int | `2` | Seconds between two attempts to take or renew the Lease. Must be below `renewDeadlineSeconds`. |
| revisionHistoryLimit | int | `5` | Number of old ReplicaSets and DaemonSet revisions to retain for rollback. |
| image.registry | string | `"ghcr.io"` | Container image registry host, prefixed to `repository`. Set empty when `repository` carries the full path. |
| image.repository | string | `"younsl/node-configuration-operator"` | Container image repository path without registry prefix. The controller and the agent run the same image. |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy of the controller and the agent pods. |
| image.tag | string | `""` | Image tag. Defaults to `.Chart.AppVersion` if empty. |
| image.digest | string | `""` | Image digest (`sha256:...`). When set, appended to the tag as `tag@digest` and takes precedence. |
| imagePullSecrets | list | `[]` | Image pull secrets of the controller and the agent pods. |
| serviceAccount.create | bool | `true` | Whether to create the controller ServiceAccount. |
| serviceAccount.annotations | object | `{}` | Annotations to add to the controller ServiceAccount. |
| serviceAccount.name | string | `""` | Controller ServiceAccount name. Defaults to the fullname template if empty. |
| serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets attached to the controller ServiceAccount, used by every pod that runs as it. |
| resources.limits.memory | string | `"64Mi"` | Memory limit of the controller. |
| resources.requests.cpu | string | `"10m"` | CPU request of the controller. |
| resources.requests.memory | string | `"32Mi"` | Memory request of the controller. |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true,"runAsNonRoot":true,"runAsUser":65532}` | Container security context of the controller. |
| priorityClassName | string | `""` | Pod priority class name of the controller. Empty uses the Kubernetes default. |
| podAnnotations | object | `{}` | Annotations to add to the controller pod. |
| nodeSelector | object | `{}` | Node selector for controller pod scheduling. |
| tolerations | list | `[]` | Tolerations for controller pod scheduling. |
| affinity | object | `{}` | Affinity rules for controller pod scheduling. |
| extraPodSpec | object | `{}` | Extra Pod spec fields of the controller, e.g. runtimeClassName. Fields the chart already renders are rejected. |
| logging.level | string | `"info"` | Log level of the controller and the agent pods: trace, debug, info, warn, error. `RUST_LOG` on the pod overrides it. |
| logging.format | string | `"json"` | Log format: json or text |
| agent.serviceAccount.create | bool | `true` | Whether to create the ServiceAccount the agent pods run as. |
| agent.serviceAccount.annotations | object | `{}` | Annotations to add to the agent ServiceAccount. |
| agent.serviceAccount.name | string | `""` | Agent ServiceAccount name. Defaults to the fullname template with an `-agent` suffix if empty. |
| agent.serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets attached to the agent ServiceAccount, used by every pod that runs as it. |
| agent.resources.limits.memory | string | `"64Mi"` | Memory limit of the agent. |
| agent.resources.requests.cpu | string | `"10m"` | CPU request of the agent. |
| agent.resources.requests.memory | string | `"16Mi"` | Memory request of the agent. |
| agent.allowScripts | bool | `false` | Whether rules with a `script` action may run. It makes the agent pod privileged with the host PID namespace and replaces `agent.securityContext`, because a script runs as root in the host root filesystem. Left off, a script rule fails and every other rule still runs. |
| agent.resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | In-place resize policy of the agent container. The default resizes CPU without a restart and restarts the container on a memory resize. |
| agent.securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"add":["CHOWN","DAC_OVERRIDE","FOWNER"],"drop":["ALL"]},"privileged":false,"readOnlyRootFilesystem":true,"runAsGroup":0,"runAsNonRoot":false,"runAsUser":0,"seccompProfile":{"type":"RuntimeDefault"}}` | Container security context of the agent, unused when `agent.allowScripts` is on. It writes host files as root, and the three capabilities cover modes and owners of files that belong to other accounts. |
| agent.updateStrategy | object | `{"rollingUpdate":{"maxUnavailable":"10%"},"type":"RollingUpdate"}` | Update strategy of the agent DaemonSet itself, used when the image, these settings, or the set of targeted nodes changes. Rule changes roll out per NodeConfiguration instead. |
| agent.priorityClassName | string | `""` | Pod priority class name of the agent. Empty uses the Kubernetes default. |
| agent.podAnnotations | object | `{}` | Annotations to add to the agent pods. |
| agent.nodeSelector | object | `{}` | Node selector of the agent pods, on top of the node affinity the controller adds. |
| agent.affinity | object | `{}` | Extra affinity of the agent pods. The controller sets the required node affinity from the targets of the NodeConfiguration resources and replaces one given here. |
| agent.tolerations | list | `[{"operator":"Exists"}]` | Tolerations of the agent pods. The default tolerates every taint so a tainted node is still configured. |
| agent.extraPodSpec | object | `{}` | Extra Pod spec fields of the agent, e.g. runtimeClassName. Fields the chart already renders are rejected. |
| nodeConfigurations | list | `[]` | NodeConfiguration custom resources to create. One agent pod per node applies all of them, and a change of the rules of one rolls out under its own strategy. |
| extraObjects | list | `[]` | Additional Kubernetes resources to create alongside the chart. |

## Source Code

* <https://github.com/younsl/addons/tree/main/box/addons/node-configuration-operator>

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
