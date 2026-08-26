# istio-waypoints

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

A Helm chart for managing Istio ambient waypoint proxies. Declares one Gateway per waypoint with its parametersRef ConfigMap, Telemetry and AuthorizationPolicy, so many waypoints across namespaces are managed from a single values file.

**Homepage:** <https://istio.io/latest/docs/ambient/usage/waypoint/>

## Requirements

Kubernetes: `>=1.30.0-0`

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/istio-waypoints
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `istio-waypoints`:

```console
helm install istio-waypoints oci://ghcr.io/younsl/charts/istio-waypoints
```

Install with custom values:

```console
helm install istio-waypoints oci://ghcr.io/younsl/charts/istio-waypoints -f values.yaml
```

Install a specific version:

```console
helm install istio-waypoints oci://ghcr.io/younsl/charts/istio-waypoints --version 0.1.0
```

### Install from local chart

Download istio-waypoints chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/istio-waypoints --untar --version 0.1.0
helm install istio-waypoints ./istio-waypoints
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade istio-waypoints oci://ghcr.io/younsl/charts/istio-waypoints
```

## Uninstall

```console
helm uninstall istio-waypoints
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | chart name | String to partially override the chart name. |
| fullnameOverride | string | release name | String to fully override the release name. |
| commonLabels | object | `{}` | Common labels to add to every resource rendered by this chart. |
| commonAnnotations | object | `{}` | Common annotations to add to every resource rendered by this chart. |
| defaults | object | `{"annotations":{},"authorizationPolicies":{},"container":{"env":[],"lifecycle":{"preStop":{"exec":{"command":["/bin/sh","-c","sleep 60"]}}},"name":"istio-proxy","resizePolicy":[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}],"resources":{"limits":{"memory":"1Gi"},"requests":{"cpu":"100m","memory":"128Mi"}}},"enabled":true,"gatewayApiVersion":"gateway.networking.k8s.io/v1","gatewayClassName":"istio-waypoint","infrastructure":{"annotations":{},"labels":{}},"labels":{},"listener":{"name":"mesh","port":15008,"protocol":"HBONE"},"name":null,"parameters":{"deployment":{"spec":{"template":{"spec":{"dnsConfig":{"options":[{"name":"ndots","value":"2"}]},"priorityClassName":"system-cluster-critical","terminationGracePeriodSeconds":70,"tolerations":[{"effect":"NoExecute","key":"node.kubernetes.io/not-ready","operator":"Exists","tolerationSeconds":300},{"effect":"NoExecute","key":"node.kubernetes.io/unreachable","operator":"Exists","tolerationSeconds":300}],"topologySpreadConstraints":[{"maxSkew":1,"topologyKey":"topology.kubernetes.io/zone","whenUnsatisfiable":"ScheduleAnyway"},{"maxSkew":1,"topologyKey":"kubernetes.io/hostname","whenUnsatisfiable":"ScheduleAnyway"}]}}}},"horizontalPodAutoscaler":{"spec":{"maxReplicas":5,"metrics":[{"resource":{"name":"cpu","target":{"averageUtilization":70,"type":"Utilization"}},"type":"Resource"}],"minReplicas":2}},"podDisruptionBudget":{"spec":{"minAvailable":1}},"service":{},"serviceAccount":{}},"revision":null,"telemetry":{"accessLogging":[{"providers":[{"name":"envoy"}]}],"enabled":true,"metrics":[],"tracing":[]},"waypointFor":"service"}` | Defaults deep-merged under every `waypoints` entry: maps merge key by key, lists are replaced as a whole. Set a default map to `null` in an entry to drop it (`{}` keeps it). |
| defaults.enabled | bool | `true` | Render this waypoint. |
| defaults.name | string | map key | Gateway name inside the namespace. This is the value workloads reference in their `istio.io/use-waypoint` label. Defaults to the map key of the waypoint entry. |
| defaults.waypointFor | string | `"service"` | Traffic the waypoint handles, set as the `istio.io/waypoint-for` label. One of `service`, `workload`, `all`, `none`. |
| defaults.revision | string | `nil` | Istio control plane revision (`istio.io/rev` label). Leave empty to use the default revision. |
| defaults.gatewayApiVersion | string | `"gateway.networking.k8s.io/v1"` | apiVersion of the rendered Gateway resource. Use `gateway.networking.k8s.io/v1beta1` on clusters whose Gateway API CRDs do not serve `v1` yet. |
| defaults.gatewayClassName | string | `"istio-waypoint"` | GatewayClass that implements the waypoint. Istio ships `istio-waypoint`. |
| defaults.listener | object | `{"name":"mesh","port":15008,"protocol":"HBONE"}` | Single HBONE listener of the waypoint. Do not change name, port or protocol; `allowedRoutes` governs only routes attached to the Gateway itself (see notes below). |
| defaults.labels | object | `{}` | Labels added to the Gateway and every companion resource of this waypoint. |
| defaults.annotations | object | `{}` | Annotations added to the Gateway and every companion resource of this waypoint. |
| defaults.infrastructure | object | `{"annotations":{},"labels":{}}` | `Gateway.spec.infrastructure`. `labels` and `annotations` are propagated by Istio to the generated Deployment, Pods and Service. |
| defaults.container | object | `{"env":[],"lifecycle":{"preStop":{"exec":{"command":["/bin/sh","-c","sleep 60"]}}},"name":"istio-proxy","resizePolicy":[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}],"resources":{"limits":{"memory":"1Gi"},"requests":{"cpu":"100m","memory":"128Mi"}}}` | Patch for the `istio-proxy` container of the waypoint Deployment. Rendered into `parameters.deployment` unless that patch already declares `containers`. |
| defaults.parameters | object | `{"deployment":{"spec":{"template":{"spec":{"dnsConfig":{"options":[{"name":"ndots","value":"2"}]},"priorityClassName":"system-cluster-critical","terminationGracePeriodSeconds":70,"tolerations":[{"effect":"NoExecute","key":"node.kubernetes.io/not-ready","operator":"Exists","tolerationSeconds":300},{"effect":"NoExecute","key":"node.kubernetes.io/unreachable","operator":"Exists","tolerationSeconds":300}],"topologySpreadConstraints":[{"maxSkew":1,"topologyKey":"topology.kubernetes.io/zone","whenUnsatisfiable":"ScheduleAnyway"},{"maxSkew":1,"topologyKey":"kubernetes.io/hostname","whenUnsatisfiable":"ScheduleAnyway"}]}}}},"horizontalPodAutoscaler":{"spec":{"maxReplicas":5,"metrics":[{"resource":{"name":"cpu","target":{"averageUtilization":70,"type":"Utilization"}},"type":"Resource"}],"minReplicas":2}},"podDisruptionBudget":{"spec":{"minAvailable":1}},"service":{},"serviceAccount":{}}` | `parametersRef` ConfigMap content. Istio applies every key as a strategic merge patch to the resource it generates. Accepted keys: `deployment`, `service`, `serviceAccount`, `horizontalPodAutoscaler`, `podDisruptionBudget`. |
| defaults.telemetry | object | `{"accessLogging":[{"providers":[{"name":"envoy"}]}],"enabled":true,"metrics":[],"tracing":[]}` | Istio Telemetry resource targeting the waypoint Gateway. |
| defaults.authorizationPolicies | object | `{}` | Map of Istio AuthorizationPolicy resources targeting the waypoint Gateway, keyed by suffix (rendered as `<waypoint>-<key>`). Enforced for every workload enrolled in the waypoint. |
| waypoints | object | `{}` | Map of waypoints keyed by name (used as Gateway name unless `name` is set). Each entry requires `namespace` and may override any field of `defaults`. |
| extraObjects | list | `[]` | Extra Kubernetes objects rendered as-is through `tpl`. |

## Source Code

* <https://github.com/istio/istio>
* <https://github.com/younsl/o/tree/main/box/kubernetes/charts/istio-waypoints>

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
