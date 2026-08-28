# trivy-collector

![Version: 0.12.0](https://img.shields.io/badge/Version-0.12.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 1.7.0](https://img.shields.io/badge/AppVersion-1.7.0-informational?style=flat-square)

Multi-cluster Trivy report collector and viewer

**Homepage:** <https://github.com/younsl/o>

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/trivy-collector
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `trivy-collector`:

```console
helm install trivy-collector oci://ghcr.io/younsl/charts/trivy-collector
```

Install with custom values:

```console
helm install trivy-collector oci://ghcr.io/younsl/charts/trivy-collector -f values.yaml
```

Install a specific version:

```console
helm install trivy-collector oci://ghcr.io/younsl/charts/trivy-collector --version 0.12.0
```

### Install from local chart

Download trivy-collector chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/trivy-collector --untar --version 0.12.0
helm install trivy-collector ./trivy-collector
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade trivy-collector oci://ghcr.io/younsl/charts/trivy-collector
```

## Uninstall

```console
helm uninstall trivy-collector
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | `""` | Override the name of the chart |
| fullnameOverride | string | `""` | Override the full name of the chart |
| replicaCount | int | `1` | Default replica count (deprecated, use server.replicaCount for UI pods; scraper always runs as a single replica to avoid split-brain writes). |
| revisionHistoryLimit | int | `10` | Number of old ReplicaSets to retain for rollback |
| image | object | `{"pullPolicy":"IfNotPresent","registry":"ghcr.io","repository":"younsl/trivy-collector","tag":""}` | Container image configuration |
| image.registry | string | `"ghcr.io"` | Container image registry host |
| image.repository | string | `"younsl/trivy-collector"` | Container image repository path without registry prefix |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy |
| image.tag | string | `""` | Image tag (defaults to chart appVersion) |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries |
| serviceAccount | object | `{"annotations":{},"automount":true,"create":true,"name":""}` | ServiceAccount configuration |
| serviceAccount.create | bool | `true` | Create a ServiceAccount |
| serviceAccount.automount | bool | `true` | Automount service account token |
| serviceAccount.annotations | object | `{}` | Annotations to add to the ServiceAccount |
| serviceAccount.name | string | `""` | Name of the ServiceAccount (auto-generated if empty) |
| podAnnotations | object | `{}` | Annotations to add to the pod |
| podLabels | object | `{}` | Labels to add to the pod |
| podSecurityContext | object | `{"fsGroup":1000,"runAsGroup":1000,"runAsNonRoot":true,"runAsUser":1000}` | Pod security context configuration |
| podSecurityContext.runAsNonRoot | bool | `true` | Run as non-root user |
| podSecurityContext.runAsUser | int | `1000` | User ID to run as |
| podSecurityContext.runAsGroup | int | `1000` | Group ID to run as |
| podSecurityContext.fsGroup | int | `1000` | Filesystem group ID |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context configuration |
| securityContext.allowPrivilegeEscalation | bool | `false` | Disallow privilege escalation |
| securityContext.capabilities | object | `{"drop":["ALL"]}` | Capabilities to drop |
| securityContext.readOnlyRootFilesystem | bool | `true` | Mount root filesystem as read-only |
| clusterName | string | `"local"` | Cluster name identifier recorded on the Hub's own reports. |
| scraper.namespaces | list | `[]` | Namespaces the local watcher scans on the Hub's own cluster (empty = all). |
| scraper.watchLocal | bool | `true` | Enable the local-cluster watcher (Hub itself). Set to false if Trivy Operator is not installed on the central cluster. |
| scraper.collect | object | `{"sbomReports":true,"vulnerabilityReports":true}` | Report kinds to watch. Applies to the Hub and to every registered edge cluster. Disabling a kind stops its watch everywhere and excludes it from hydration accounting, so readiness still clears. SbomReports are the bulk of the data, so turning them off is the main lever on database size. |
| scraper.collect.vulnerabilityReports | bool | `true` | Watch VulnerabilityReports. |
| scraper.collect.sbomReports | bool | `true` | Watch SbomReports. |
| scraper.storage | object | `{"medium":"","mountPath":"/data","sizeLimit":"2Gi"}` | Ephemeral storage for the report database. Reports are a mirror of the CRs that exist right now, so an empty start costs one relist and nothing else, and it prunes the rows a deleted CR used to leave behind forever. |
| scraper.storage.mountPath | string | `"/data"` | Where the database lives inside the container. |
| scraper.storage.sizeLimit | string | `"2Gi"` | emptyDir sizeLimit. Must cover the database plus the WAL and rebuild churn that can briefly double it. Keep it in step with the container's ephemeral-storage limit below. |
| scraper.storage.medium | string | `""` | emptyDir medium. Empty uses node disk; "Memory" trades node memory for speed and counts against the pod's memory limit. |
| scraper.strategy | object | `{"rollingUpdate":{"maxSurge":1,"maxUnavailable":0},"type":"RollingUpdate"}` | Deployment strategy. RollingUpdate is safe now that each pod owns its own emptyDir and reports unready until the fleet is hydrated: the outgoing pod keeps serving reads for the whole rebuild. |
| scraper.readinessProbe | object | `{"failureThreshold":60,"httpGet":{"path":"/readyz","port":"health"},"initialDelaySeconds":5,"periodSeconds":10,"timeoutSeconds":5}` | Readiness probe. Stays failing until every registered cluster has replayed its initial list, so the server is never routed to a partial report set. failureThreshold is generous because a large fleet's first sync takes minutes. |
| scraper.resources | object | `{"limits":{"ephemeral-storage":"2Gi","memory":"512Mi"},"requests":{"cpu":"50m","ephemeral-storage":"2Gi","memory":"256Mi"}}` | Resource requests and limits. The scraper now holds the SQLite page cache and builds query results, so memory moved here from the server. ephemeral-storage must be declared because the database sits on an emptyDir drawn from the node. |
| scraper.resizePolicy | list | `[]` | Container resize policy for in-place resource updates. |
| scraper.nodeSelector | object | `{}` | Node selector. |
| scraper.tolerations | list | `[]` | Tolerations. |
| scraper.affinity | object | `{}` | Affinity rules. |
| scraper.topologySpreadConstraints | list | `[]` | Topology spread constraints. |
| scraper.serviceMonitor | object | `{"additionalLabels":{},"annotations":{},"enabled":false,"interval":"30s","scrapeTimeout":""}` | ServiceMonitor for scraper (requires Prometheus Operator). |
| scraper.serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor for the scraper pod. |
| scraper.serviceMonitor.interval | string | `"30s"` | Scrape interval. |
| scraper.serviceMonitor.scrapeTimeout | string | `""` | Scrape timeout (defaults to Prometheus global when empty). |
| scraper.serviceMonitor.additionalLabels | object | `{}` | Additional labels to attach to the ServiceMonitor. |
| scraper.serviceMonitor.annotations | object | `{}` | Annotations to attach to the ServiceMonitor. |
| server.replicaCount | int | `1` | Replica count for the UI / API pod. More than one is supported now that this tier is stateless; set server.mcp.stateless when raising it. |
| server.podDisruptionBudget | object | `{"enabled":true,"maxUnavailable":1,"minAvailable":""}` | PodDisruptionBudget for the UI tier. Only meaningful with more than one replica, and skipped otherwise. |
| server.podDisruptionBudget.enabled | bool | `true` | Create a PodDisruptionBudget. |
| server.podDisruptionBudget.minAvailable | string | `""` | Minimum available pods. Takes precedence when set. |
| server.podDisruptionBudget.maxUnavailable | int | `1` | Maximum unavailable pods. |
| server.port | int | `3000` | HTTP server port |
| server.externalUrl | string | `""` | Explicit external URL used to render "View report" deep links in outbound notifications (e.g. `https://trivy.example.com`). Leave empty to auto-derive from gateway.hostnames. |
| server.resources | object | `{"limits":{"memory":"96Mi"},"requests":{"cpu":"20m","memory":"48Mi"}}` | Resource requests and limits. Lower than before: this pod no longer holds a connection pool or materializes query results from a local file, so that memory moved to the scraper. |
| server.resizePolicy | list | `[]` | Container resize policy for in-place resource updates. |
| server.nodeSelector | object | `{}` | Node selector. |
| server.tolerations | list | `[]` | Tolerations. |
| server.affinity | object | `{}` | Affinity rules. |
| server.topologySpreadConstraints | list | `[]` | Topology spread constraints. |
| server.mcp | object | `{"allowedHosts":[],"enabled":false,"stateless":false}` | Embedded MCP (Model Context Protocol) server for LLM agents such as kagent. Served by the server pod at `/mcp` over the Streamable HTTP transport and gated by the same auth and RBAC as the REST API. |
| server.mcp.enabled | bool | `false` | Mount the MCP endpoint at `/mcp`. |
| server.mcp.allowedHosts | list | `[]` | Allowed `Host` header values for `/mcp`. Leave empty to disable Host validation, which is required for in-cluster access via Service DNS. |
| server.mcp.stateless | bool | `false` | Serve MCP without sessions. Set true when server.replicaCount > 1. |
| server.serviceMonitor | object | `{"additionalLabels":{},"annotations":{},"enabled":false,"interval":"30s","scrapeTimeout":""}` | ServiceMonitor for server (requires Prometheus Operator). |
| server.serviceMonitor.enabled | bool | `false` | Create a ServiceMonitor for the server pod. |
| server.serviceMonitor.interval | string | `"30s"` | Scrape interval. |
| server.serviceMonitor.scrapeTimeout | string | `""` | Scrape timeout (defaults to Prometheus global when empty). |
| server.serviceMonitor.additionalLabels | object | `{}` | Additional labels to attach to the ServiceMonitor. |
| server.serviceMonitor.annotations | object | `{}` | Annotations to attach to the ServiceMonitor. |
| server.gateway | object | `{"enabled":false,"hostnames":["trivy.example.com"],"name":"","parentRefs":[{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"main-gateway","namespace":"gateway-system","sectionName":"https"}],"rules":[{"backendRefs":[{"group":"","kind":"Service","name":"","port":3000,"weight":1}],"filters":[],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]}` | Gateway API HTTPRoute configuration. The only ingress path this chart supports; plain Ingress was removed in favour of Gateway API. |
| server.gateway.enabled | bool | `false` | Enable HTTPRoute |
| server.gateway.name | string | `""` | HTTPRoute name (defaults to fullname) |
| server.gateway.parentRefs | list | `[{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"main-gateway","namespace":"gateway-system","sectionName":"https"}]` | Parent Gateway references |
| server.gateway.hostnames | list | `["trivy.example.com"]` | Hostnames for the route |
| server.gateway.rules | list | `[{"backendRefs":[{"group":"","kind":"Service","name":"","port":3000,"weight":1}],"filters":[],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]` | HTTP route rules |
| server.gateway.rules[0].filters | list | `[]` | HTTPRoute filters (RequestHeaderModifier, ResponseHeaderModifier, RequestRedirect, URLRewrite, RequestMirror, ExtensionRef) |
| server.gateway.rules[0].backendRefs | list | `[{"group":"","kind":"Service","name":"","port":3000,"weight":1}]` | HTTPBackendRefs. Each entry is rendered verbatim (group, kind, name, port, weight). Empty `name` defaults to the server pod's Service at render time. |
| server.auth | object | `{"mode":"none","rbac":{"defaultPolicy":"role:readonly","policy":"p, role:readonly, reports, get, allow\np, role:readonly, clusters, get, allow\np, role:readonly, stats, get, allow\np, role:readonly, tokens, get, allow\np, role:readonly, tokens, create, allow\np, role:admin, *, *, allow\ng, admin, role:admin\n"},"sso":{"clientId":{"key":"client-id","name":"","value":""},"clientSecret":{"key":"client-secret","name":"","value":""},"issuer":"","redirectUrl":"","scopes":["openid","profile","email","groups"]}}` | Authentication configuration (server mode only) |
| server.auth.mode | string | `"none"` | Authentication mode: "none" (anonymous) or "keycloak" (OIDC) |
| server.auth.rbac | object | `{"defaultPolicy":"role:readonly","policy":"p, role:readonly, reports, get, allow\np, role:readonly, clusters, get, allow\np, role:readonly, stats, get, allow\np, role:readonly, tokens, get, allow\np, role:readonly, tokens, create, allow\np, role:admin, *, *, allow\ng, admin, role:admin\n"}` | RBAC configuration (ArgoCD-style CSV policy) |
| server.auth.rbac.policy | string | See values | RBAC policy CSV (ArgoCD format) |
| server.auth.rbac.defaultPolicy | string | `"role:readonly"` | Default RBAC policy for authenticated users without explicit group bindings |
| server.auth.sso | object | `{"clientId":{"key":"client-id","name":"","value":""},"clientSecret":{"key":"client-secret","name":"","value":""},"issuer":"","redirectUrl":"","scopes":["openid","profile","email","groups"]}` | SSO configuration (only used when auth.mode is "keycloak") |
| server.auth.sso.issuer | string | `""` | OIDC issuer URL (e.g., https://keycloak.example.com/realms/trivy) |
| server.auth.sso.clientId | object | `{"key":"client-id","name":"","value":""}` | OIDC client ID configuration |
| server.auth.sso.clientId.value | string | `""` | Plaintext client ID value (used if set, takes precedence over secret reference) |
| server.auth.sso.clientId.name | string | `""` | Secret name containing client ID |
| server.auth.sso.clientId.key | string | `"client-id"` | Secret key for client ID |
| server.auth.sso.clientSecret | object | `{"key":"client-secret","name":"","value":""}` | OIDC client secret configuration |
| server.auth.sso.clientSecret.value | string | `""` | Plaintext client secret value (used if set, takes precedence over secret reference) |
| server.auth.sso.clientSecret.name | string | `""` | Secret name containing client secret |
| server.auth.sso.clientSecret.key | string | `"client-secret"` | Secret key for client secret |
| server.auth.sso.redirectUrl | string | `""` | OIDC redirect URL (full callback URL, e.g., https://trivy.example.com/auth/callback) |
| server.auth.sso.scopes | list | `["openid","profile","email","groups"]` | OIDC scopes |
| service | object | `{"annotations":{},"port":3000,"type":"ClusterIP"}` | Service configuration |
| service.type | string | `"ClusterIP"` | Service type |
| service.port | int | `3000` | Service port |
| service.annotations | object | `{}` | Annotations to add to the Service |
| internal | object | `{"existingSecret":"","networkPolicy":{"enabled":true,"extraFrom":[]},"port":8081,"secretKey":"token","token":""}` | Internal API between the scraper (which owns the database) and the server pods. It returns every report in the fleet with no per-user filtering, because RBAC is applied above it in the server, so it is never exposed through the Ingress, the HTTPRoute, or a ServiceMonitor. |
| internal.port | int | `8081` | Port the scraper serves the internal read API on. |
| internal.token | string | `""` | Shared token, compared in constant time on every internal request. Leave empty to generate one on first install and keep it across upgrades. A GitOps controller that renders without cluster access cannot read the existing value, so set this (or internal.existingSecret) there. |
| internal.existingSecret | string | `""` | Use an existing Secret for the token instead of managing one. |
| internal.secretKey | string | `"token"` | Key inside the token Secret. |
| internal.networkPolicy | object | `{"enabled":true,"extraFrom":[]}` | NetworkPolicy fencing the internal port to the server pods. A second fence alongside the shared token. |
| internal.networkPolicy.enabled | bool | `true` | Create the NetworkPolicy. On a cluster whose CNI does not enforce policy this is inert rather than harmful, so it defaults on. |
| internal.networkPolicy.extraFrom | list | `[]` | Additional `from` selectors allowed to reach the internal port. |
| health | object | `{"port":8080}` | Health check configuration |
| health.port | int | `8080` | Health check server port |
| logging | object | `{"format":"json","level":"info"}` | Logging configuration |
| logging.format | string | `"json"` | Log format: "json" or "pretty" |
| logging.level | string | `"info"` | Log level: trace, debug, info, warn, error |
| livenessProbe | object | `{"failureThreshold":3,"httpGet":{"path":"/healthz","port":"health"},"initialDelaySeconds":10,"periodSeconds":30,"timeoutSeconds":5}` | Liveness probe configuration |
| livenessProbe.initialDelaySeconds | int | `10` | Initial delay before starting probes |
| livenessProbe.periodSeconds | int | `30` | Probe interval |
| livenessProbe.timeoutSeconds | int | `5` | Probe timeout |
| livenessProbe.failureThreshold | int | `3` | Number of failures before marking unhealthy |
| readinessProbe | object | `{"failureThreshold":3,"httpGet":{"path":"/readyz","port":"health"},"initialDelaySeconds":5,"periodSeconds":10,"timeoutSeconds":5}` | Readiness probe configuration |
| readinessProbe.initialDelaySeconds | int | `5` | Initial delay before starting probes |
| readinessProbe.periodSeconds | int | `10` | Probe interval |
| readinessProbe.timeoutSeconds | int | `5` | Probe timeout |
| readinessProbe.failureThreshold | int | `3` | Number of failures before marking not ready |
| dnsPolicy | string | "" | DNS policy for the pod (ClusterFirst, ClusterFirstWithHostNet, Default, None) |
| dnsConfig | object | {} | DNS configuration for the pod |
| migration | object | `{"exportState":{"affinity":{},"backoffLimit":2,"dbPath":"/data/trivy.db","dryRun":false,"enabled":false,"existingClaim":"","nodeSelector":{},"resources":{"limits":{"memory":"128Mi"},"requests":{"cpu":"50m","memory":"64Mi"}},"tolerations":[],"ttlSecondsAfterFinished":3600}}` | One-shot migration off the PersistentVolume. Reports need no export, the next scraper start relists them, but API tokens and report notes are unrecoverable, so they are read out of the legacy database and written to a Secret and a ConfigMap before the volume goes away.  Run with exportState.enabled and the old PVC name, verify both objects, then disable it and delete the PVC. Until then the PVC is the rollback. |
| migration.exportState.enabled | bool | `false` | Run the export as a pre-install/pre-upgrade hook Job. |
| migration.exportState.existingClaim | string | `""` | Name of the existing PVC holding the legacy database. |
| migration.exportState.dbPath | string | `"/data/trivy.db"` | Path to the legacy database inside that volume. |
| migration.exportState.dryRun | bool | `false` | Report what would be written without writing it. |
| migration.exportState.backoffLimit | int | `2` | Job backoff limit. |
| migration.exportState.ttlSecondsAfterFinished | int | `3600` | Seconds to keep the finished Job. |
| migration.exportState.nodeSelector | object | `{}` | Node selector. Defaults to the scraper's, because the legacy PVC is ReadWriteOnce and this Job can only mount it where the scraper already has it attached. |
| migration.exportState.tolerations | list | `[]` | Tolerations. Defaults to the scraper's. Without them a Job on a tainted node stays Pending, and a pending pre-sync hook blocks the upgrade it was meant to precede. |
| migration.exportState.affinity | object | `{}` | Affinity rules. Defaults to the scraper's. |
| migration.exportState.resources | object | `{"limits":{"memory":"128Mi"},"requests":{"cpu":"50m","memory":"64Mi"}}` | Resource requests and limits. |
| extraObjects | list | [] | Extra Kubernetes objects to deploy alongside the chart |

## Source Code

* <https://github.com/younsl/o/tree/main/box/kubernetes/trivy-collector>

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
