# backstage-mcp

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.1.0](https://img.shields.io/badge/AppVersion-0.1.0-informational?style=flat-square)

Read-only MCP server exposing the Backstage catalog, search, TechDocs and in-house plugins to AI agents such as kagent

**Homepage:** <https://github.com/younsl/o>

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/backstage-mcp
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `backstage-mcp`:

```console
helm install backstage-mcp oci://ghcr.io/younsl/charts/backstage-mcp
```

Install with custom values:

```console
helm install backstage-mcp oci://ghcr.io/younsl/charts/backstage-mcp -f values.yaml
```

Install a specific version:

```console
helm install backstage-mcp oci://ghcr.io/younsl/charts/backstage-mcp --version 0.1.0
```

### Install from local chart

Download backstage-mcp chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/backstage-mcp --untar --version 0.1.0
helm install backstage-mcp ./backstage-mcp
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade backstage-mcp oci://ghcr.io/younsl/charts/backstage-mcp
```

## Uninstall

```console
helm uninstall backstage-mcp
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| image.registry | string | `"ghcr.io"` | Container image registry host |
| image.repository | string | `"younsl/backstage-mcp"` | Container image repository path without registry prefix |
| image.tag | string | `""` | Image tag; defaults to the chart appVersion when empty |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries |
| nameOverride | string | `""` | Override the chart name |
| fullnameOverride | string | `""` | Override the fully qualified release name |
| replicaCount | int | `1` | Number of replicas. The server is stateless (every MCP request is served on its own), so replicas can be raised freely. |
| revisionHistoryLimit | int | `3` | Number of old ReplicaSets to retain for rollback |
| strategy | object | `{"rollingUpdate":{"maxSurge":"25%","maxUnavailable":"25%"},"type":"RollingUpdate"}` | Deployment update strategy |
| strategy.rollingUpdate.maxSurge | string|int | `"25%"` | Max Pods created above desired count during an update |
| strategy.rollingUpdate.maxUnavailable | string|int | `"25%"` | Max Pods unavailable during an update |
| backstage.url | string | `"http://backstage.backstage.svc:7007"` | Backstage backend base URL the tools read from, typically the in-cluster Service of the official Backstage chart |
| backstage.existingSecret | string | `""` | Name of an existing Secret holding the Backstage external-access token. Leave empty to create one from `backstage.token`. |
| backstage.existingSecretKey | string | `"BACKSTAGE_TOKEN"` | Key inside the Secret holding the Backstage token |
| backstage.token | string | `""` | Static token declared under `backend.auth.externalAccess` in the Backstage app-config. Only used when `backstage.existingSecret` is empty; prefer an externally managed Secret. |
| backstage.requestTimeoutSeconds | int | `30` | Timeout in seconds for one request to Backstage |
| mcp.path | string | `"/mcp"` | Path the MCP endpoint is served on |
| mcp.maxResultChars | int | `100000` | Upper bound on the characters of one tool result; longer payloads are cut with a note asking the model to narrow the query |
| mcp.existingSecret | string | `""` | Name of an existing Secret holding the bearer token MCP clients must present. Leave empty to create one from `mcp.bearerToken`, or leave both empty to serve the endpoint without authentication. |
| mcp.existingSecretKey | string | `"MCP_BEARER_TOKEN"` | Key inside the Secret holding the raw bearer token |
| mcp.existingSecretAuthorizationKey | string | `"AUTHORIZATION"` | Key inside the Secret holding the full `Authorization` header value (`Bearer <token>`), which is what a kagent RemoteMCPServer reads through `headersFrom`. The chart-managed Secret always carries it. |
| mcp.bearerToken | string | `""` | Bearer token MCP clients must present. Only used when `mcp.existingSecret` is empty. |
| kagent.remoteMCPServer.enabled | bool | `false` | Create a kagent RemoteMCPServer pointing at this Service so Agents can reference the tools by name. The resource is created in the release namespace, and kagent resolves `headersFrom` Secrets in the Agent's namespace, so install the chart where the Agents live or copy the bearer Secret there. |
| kagent.remoteMCPServer.apiVersion | string | `"kagent.dev/v1alpha3"` | API version of the RemoteMCPServer resource |
| kagent.remoteMCPServer.name | string | `""` | Name of the RemoteMCPServer; empty uses the release fullname |
| kagent.remoteMCPServer.description | string | `"Read-only Backstage catalog, search, TechDocs and plugin data"` | Description shown by kagent |
| kagent.remoteMCPServer.timeout | string | `"30s"` | Per-request timeout kagent applies to tool calls |
| kagent.remoteMCPServer.sseReadTimeout | string | `"5m"` | How long kagent keeps an idle streaming response open |
| kagent.remoteMCPServer.allowedNamespaces | object | `{}` | `spec.allowedNamespaces` to let Agents in other namespaces reference the server, passed through as written |
| log.level | string | `"info"` | Log level: debug, info, warn, error |
| log.format | string | `"json"` | Log format: json or text |
| extraEnv | list | `[]` | Extra environment variables for the container |
| extraEnvFrom | list | `[]` | Extra `envFrom` sources for the container |
| ports.http | int | `8080` | Port the MCP endpoint and probes listen on |
| serviceAccount.create | bool | `true` | Create a ServiceAccount |
| serviceAccount.name | string | `""` | ServiceAccount name; empty uses the release fullname |
| serviceAccount.annotations | object | `{}` | Annotations for the ServiceAccount |
| serviceAccount.automountServiceAccountToken | bool | `false` | Mount the ServiceAccount token into the pod. The server never calls the Kubernetes API, so it is off. |
| serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets attached to the ServiceAccount |
| service.enabled | bool | `true` | Create a Service in front of the pods |
| service.type | string | `"ClusterIP"` | Service type |
| service.trafficDistribution | string | `""` | `spec.trafficDistribution`, for example PreferClose; empty omits the field |
| resources | object | `{"limits":{"memory":"64Mi"},"requests":{"cpu":"25m","memory":"32Mi"}}` | Pod resource requests and limits |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | Container resize policy for in-place vertical scaling (requires Kubernetes 1.27+); empty omits the field |
| terminationGracePeriodSeconds | int | `30` | Grace period for shutdown; in-flight tool calls are given five seconds to finish |
| podAnnotations | object | `{}` | Extra annotations for the pod |
| podLabels | object | `{}` | Extra labels for the pod |
| podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod-level security context |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container-level security context |
| nodeSelector | object | `{}` | Node selector for pod scheduling |
| tolerations | list | `[]` | Tolerations for pod scheduling |
| affinity | object | `{}` | Affinity rules for pod scheduling |
| topologySpreadConstraints | list | `[]` | Topology spread constraints for pod scheduling. `labelSelector` is filled in with the chart's selector labels when a constraint omits it, so spreading replicas across zones needs only the topologyKey. |
| dnsPolicy | string | `""` | Pod DNS policy, e.g. ClusterFirst or None; empty omits the field |
| dnsConfig | object | `{}` | Pod DNS config (used with dnsPolicy None); empty omits the field |
| extraObjects | list | `[]` | Additional Kubernetes manifests rendered verbatim |

## Source Code

* <https://github.com/younsl/o/tree/main/box/kubernetes/backstage/backstage-mcp>

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
