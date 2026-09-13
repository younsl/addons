# forklift

![Version: 0.12.3](https://img.shields.io/badge/Version-0.12.3-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.13.2](https://img.shields.io/badge/AppVersion-0.13.2-informational?style=flat-square)

Lightweight Kubernetes-native artifact repository (Maven, npm, Cargo, Go, PyPI) with proxy caching and supply-chain controls (age policy, package approval, vulnerability scanning)

**Homepage:** <https://github.com/younsl/addons/tree/main/box/kubernetes/forklift>

## Requirements

| Repository | Name | Version |
|------------|------|---------|
| https://charts.min.io/ | minio | 5.4.0 |

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/forklift
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `forklift`:

```console
helm install forklift oci://ghcr.io/younsl/charts/forklift
```

Install with custom values:

```console
helm install forklift oci://ghcr.io/younsl/charts/forklift -f values.yaml
```

Install a specific version:

```console
helm install forklift oci://ghcr.io/younsl/charts/forklift --version 0.12.3
```

### Install from local chart

Download forklift chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/forklift --untar --version 0.12.3
helm install forklift ./forklift
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade forklift oci://ghcr.io/younsl/charts/forklift
```

## Uninstall

```console
helm uninstall forklift
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| replicaCount | int | `2` | Number of replicas. With 2+ replicas, enable ha to elect a single active writer. |
| revisionHistoryLimit | int | `10` | Number of old ReplicaSets to retain for rollback. |
| strategy | object | `{}` | Deployment update strategy. Empty auto-selects: RollingUpdate for the s3 backend, Recreate for fs. |
| image.registry | string | `"ghcr.io"` | Image registry host. Empty folds the host into `repository`. |
| image.repository | string | `"younsl/forklift"` | Image repository. |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy. |
| image.tag | string | `""` | Image tag. Empty uses the chart appVersion. |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries. |
| nameOverride | string | `""` | Override the chart name portion of resource names. |
| fullnameOverride | string | `""` | Override the fully qualified resource name. |
| ha.enabled | bool | `nil` | Enable leader election. Auto-derived from replicaCount > 1 when left null. |
| ha.leaseName | string | `""` | Lease object name. Defaults to the release fullname when empty. |
| ha.leaseDuration | string | `"15s"` | Duration that non-leader candidates wait before attempting to acquire leadership. |
| ha.renewDeadline | string | `"10s"` | Deadline for the leader to renew the lease before giving up leadership. |
| ha.retryPeriod | string | `"2s"` | Interval between leadership acquisition attempts. |
| replication.enabled | bool | `false` | Enable PV-based replication (StatefulSet, per-pod RWO PVC). Mutually exclusive with shared RWX mode. |
| replication.interval | string | `"30s"` | Standby pull interval; the bounded data-loss window on failover. |
| replication.token | string | `""` | Bearer token for internal replication endpoints. Empty generates one into the chart Secret. |
| replication.podManagementPolicy | string | `"Parallel"` | StatefulSet pod management policy: Parallel or OrderedReady. |
| persistence.enabled | bool | `true` | Enable persistent storage. When false, data is lost on pod restart. |
| persistence.storageClass | string | `""` | StorageClass for the PVC. Uses the cluster default when empty. |
| persistence.accessModes | list | `["ReadWriteMany"]` | PVC access modes. MUST be ReadWriteMany for replicaCount > 1. |
| persistence.size | string | `"20Gi"` | PVC storage size. |
| persistence.annotations | object | `{}` | Annotations to add to the PVC. |
| storage.backend | string | `"fs"` | Storage backend: "fs" or "s3". Bundled MinIO (minio.enabled) forces s3. |
| storage.s3.bucket | string | `""` | S3 bucket. Required for s3 without bundled MinIO; else defaults to the first minio.buckets entry. |
| storage.s3.prefix | string | `""` | Key prefix within the bucket. |
| storage.s3.region | string | `""` | AWS region. Empty uses the AWS default chain. |
| storage.s3.endpoint | string | `""` | Custom S3 endpoint for S3-compatible stores. Auto-targets the bundled MinIO when enabled. |
| storage.s3.forcePathStyle | bool | `false` | Path-style addressing (MinIO needs it). Forced on with bundled MinIO. |
| storage.s3.metaSyncInterval | string | `"30s"` | Metadata snapshot sync cadence; the bounded data-loss window on failover. |
| storage.s3.existingSecret | string | `""` | Existing Secret with keys access-key-id and secret-access-key. Empty uses the AWS default chain (IRSA/Pod Identity). |
| minio.enabled | bool | `true` | Deploy bundled MinIO and wire forklift's s3 backend to it. |
| minio.image.repository | string | `"quay.io/minio/minio"` | MinIO server image repository. |
| minio.image.tag | string | `"RELEASE.2025-09-07T16-13-09Z"` | MinIO image tag. Pinned for the S3 conditional-write support HA fencing needs. |
| minio.mode | string | `"standalone"` | Deployment mode: "standalone" or "distributed". |
| minio.rootUser | string | `"forklift"` | MinIO root access key, mirrored as the S3 access-key-id. CHANGE for non-dev use. |
| minio.rootPassword | string | `"forklift-minio-change-me"` | MinIO root secret key (min 8 chars), mirrored as the S3 secret-access-key. CHANGE for non-dev use. |
| minio.buckets | list | `[{"name":"forklift","policy":"none","purge":false}]` | Buckets to provision. The first entry is forklift's unless storage.s3.bucket overrides it. |
| minio.persistence.enabled | bool | `true` | Persist MinIO data on a PVC. |
| minio.persistence.size | string | `"10Gi"` | Size of the MinIO data volume. |
| minio.resources.requests.memory | string | `"512Mi"` | MinIO memory request, lowered from the subchart's 16Gi default. |
| auth.anonymousRead | bool | `false` | Allow unauthenticated read (pull) access. |
| auth.sessionTTL | string | `"12h"` | Session cookie lifetime. |
| auth.sessionSecret | string | `""` | Session cookie signing secret, shared across replicas. Empty generates one into the chart Secret. |
| auth.bootstrap.adminUser | string | `"admin"` | Admin username seeded on first run. |
| auth.bootstrap.adminPassword | string | `""` | Admin password seeded on first run. Empty generates one into the chart Secret (key: bootstrap-admin-password). |
| auth.oidc.enabled | bool | `false` | Enable OIDC single sign-on. |
| auth.oidc.issuerURL | string | `""` | OIDC issuer URL. |
| auth.oidc.clientID | string | `""` | OIDC client ID. |
| auth.oidc.clientSecret | string | `""` | OIDC client secret. |
| auth.oidc.redirectURL | string | `""` | OIDC redirect URL. |
| auth.oidc.usernameClaim | string | `"preferred_username"` | Token claim used as the username. |
| auth.oidc.groupsClaim | string | `"groups"` | Token claim used for group membership. |
| auth.rbac.enabled | bool | `true` | Enable declarative RBAC. When false, roles are managed only through the UI/API. |
| auth.rbac.policyDefault | string | `"readonly"` | Default role for every authenticated user (ArgoCD policy.default). Empty means deny-all until a role is granted. |
| auth.rbac.policy | string | `"# The `administrator` role (admin on every repository) is created\n# automatically for the bootstrap admin on first run, so it is not declared\n# here; reference it in grant lines below to assign full access to others.\n\n# readonly: read-only (pull) access to every repository. Default role for\n# all authenticated users.\np, readonly, repo, read, *, allow\n\n# auditor (security engineer): read-only across all administrative surfaces\n# (audit) plus package approval decisions, security policy edits and\n# repository reads, but no create/update/delete and no access to upstream\n# credentials. Drop the `security` line to leave the Security tab\n# read-only while keeping approval rights.\np, auditor, repo, audit, *, allow\np, auditor, repo, approve, *, allow\np, auditor, repo, security, *, allow\np, auditor, repo, read, *, allow\n\n# Example: developers can pull and push to team repositories.\n# p, developer, repo, read, team-a-*, allow\n# p, developer, repo, write, team-a-*, allow\n\n# Example: map a Keycloak group and a specific user to roles.\n# g, group:/platform-admins, administrator\n# g, user:alice, developer\n"` | ArgoCD-style policy: "p, <role>, repo, <action>, <repo-glob>, allow" permissions and "g, <group:name|user:name>, <role>" grants. |
| auth.rbac.accounts | list | `[]` | Declarative local accounts. Passwords are set here or generated into the chart Secret (key: local-user-<name>-password). |
| audit.enabled | bool | `true` | Enable the audit log. |
| audit.retention | string | `"2160h"` | Retention period; the leader prunes older entries. "0" keeps them forever. |
| uiUpload.enabled | bool | `true` | Enable component-aware artifact upload through the management API and web UI. |
| uiUpload.maxDuration | string | `"30m"` | Maximum wall-clock duration of one upload request. |
| uiUpload.maxConcurrent | int | `4` | Maximum process-wide concurrent upload requests. |
| uiUpload.maxConcurrentUser | int | `2` | Maximum concurrent upload requests for one authenticated principal. |
| uiUpload.maxAssets | int | `16` | Maximum number of uploaded files in one publication. |
| uiUpload.maxFileBytes | string | `"256MiB"` | Maximum size of one non-Go uploaded file. |
| uiUpload.maxBatchBytes | string | `"512MiB"` | Maximum total file bytes in one publication. |
| uiUpload.goMaxZipBytes | string | `"500MiB"` | Maximum Go module zip size; cannot exceed the GOPROXY protocol limit. |
| vuln.osvUrl | string | `"https://api.osv.dev"` | OSV API base URL used to scan requested versions. Empty disables scanning. |
| vuln.rescanInterval | string | `"6h"` | How often stale scan results are re-queried. |
| vuln.ttl | string | `"24h"` | Age at which a scan result is considered stale. |
| vuln.workers | int | `6` | Concurrent scan workers draining the queue. |
| license.depsDevUrl | string | `"https://api.deps.dev"` | deps.dev API base URL used to resolve package licenses. Empty disables resolution. |
| license.rescanInterval | string | `"24h"` | How often stale license results are re-queried. |
| license.ttl | string | `"168h"` | Age at which a license result is considered stale. |
| license.workers | int | `6` | Concurrent resolution workers draining the queue. |
| coverageScanning.gitlab.enabled | bool | `false` | Turn coverage scanning on. Off by default: a GitLab token already in the environment must not start a crawl on its own. |
| coverageScanning.gitlab.url | string | `""` | GitLab base URL, e.g. https://gitlab.example.com. Required when enabled. |
| coverageScanning.gitlab.token | string | `""` | Access token with read_api. Written into the chart's Secret; prefer existingSecret in production. |
| coverageScanning.gitlab.existingSecret | string | `""` | Read the token from this Secret instead, so it never appears in values. |
| coverageScanning.gitlab.existingSecretKey | string | `"coverage-gitlab-token"` | Key holding the token, in either the chart's Secret or existingSecret. |
| externalUrl | string | `""` | Public base URL (e.g. https://forklift.example.com) for generated metadata URLs. Empty uses the request host. |
| seedDefaultRepos | bool | `true` | Seed default proxy and hosted repositories on first run. Idempotent. |
| log.level | string | `"info"` | Log level (debug, info, warn, error). |
| log.format | string | `"json"` | Log format (json, text). |
| serviceAccount.create | bool | `true` | Create a ServiceAccount. |
| serviceAccount.annotations | object | `{}` | Annotations to add to the ServiceAccount (e.g. IRSA role ARN). |
| serviceAccount.name | string | `""` | ServiceAccount name. Generated from the fullname when empty. |
| serviceAccount.imagePullSecrets | list | `[]` | Image pull secrets set on the ServiceAccount, separate from the pod-level imagePullSecrets. |
| rbac.create | bool | `true` | Create Role/RoleBinding for the leader-election Lease. |
| service.type | string | `"ClusterIP"` | Service type. |
| service.port | int | `80` | Service port for HTTP traffic. |
| service.metricsPort | int | `8081` | Service port exposing Prometheus metrics (container port 8081). |
| service.annotations | object | `{}` | Annotations to add to the Service. |
| service.trafficDistribution | string | `""` | Traffic distribution preference (e.g. PreferClose). Empty omits the field. |
| service.headless.annotations | object | `{}` | Annotations to add to the headless Service. |
| service.headless.trafficDistribution | string | `""` | Traffic distribution for the headless Service. Empty omits the field. |
| gateway.enabled | bool | `false` | Enable HTTPRoute. |
| gateway.name | string | `""` | HTTPRoute name. Defaults to the release fullname when empty. |
| gateway.parentRefs | list | `[{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"main-gateway","namespace":"gateway-system","sectionName":"https"}]` | Parent Gateway references. |
| gateway.hostnames | list | `["forklift.example.com"]` | Hostnames for the route. |
| gateway.rules | list | `[{"backendRefs":[{"group":"","kind":"Service","name":"","port":"","weight":1}],"filters":[],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]` | HTTP route rules. |
| gateway.rules[0].filters | list | `[]` | HTTPRoute filters. |
| gateway.rules[0].backendRefs | list | `[{"group":"","kind":"Service","name":"","port":"","weight":1}]` | HTTPBackendRefs. Empty name/port default to the chart Service and service.port. |
| podAnnotations | object | `{}` | Annotations to add to pods. |
| podLabels | object | `{}` | Labels to add to pods. |
| podSecurityContext | object | `{"fsGroup":65532,"runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532}` | Pod-level security context. |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container-level security context. |
| resources | object | `{"limits":{"memory":"256Mi"},"requests":{"cpu":"50m","memory":"128Mi"}}` | Container resource requests and limits. |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | In-place vertical scaling policy: CPU in place, restart on memory resize. |
| livenessProbe | object | `{"httpGet":{"path":"/healthz","port":"http"},"initialDelaySeconds":5,"periodSeconds":10}` | Liveness probe configuration. |
| readinessProbe | object | `{"httpGet":{"path":"/readyz","port":"http"},"initialDelaySeconds":3,"periodSeconds":5}` | Readiness probe configuration. |
| podDisruptionBudget.enabled | bool | `true` | Create a PodDisruptionBudget to keep replicas available during disruptions. |
| podDisruptionBudget.minAvailable | int | `1` | Minimum number of available replicas. |
| podDisruptionBudget.unhealthyPodEvictionPolicy | string | `"AlwaysAllow"` | Eviction policy for unhealthy pods: AlwaysAllow or IfHealthyBudget. |
| mcp.enabled | bool | `false` | Deploy the forklift-mcp server as its own pod. |
| mcp.image.registry | string | `"ghcr.io"` | MCP image registry host. Set empty to fold the host into `repository`. |
| mcp.image.repository | string | `"younsl/forklift-mcp"` | MCP image repository (path under the registry). |
| mcp.image.pullPolicy | string | `"IfNotPresent"` | MCP image pull policy. |
| mcp.image.tag | string | `"0.3.1"` | MCP image tag. Released independently of forklift, so no appVersion fallback. |
| mcp.replicaCount | int | `1` | Number of MCP replicas. Keep 1: MCP sessions live in pod memory. |
| mcp.upstreamURL | string | `""` | forklift API base URL to proxy to. Empty targets this release's Service. |
| mcp.token | object | `{"existingSecret":"","key":"token"}` | Optional fallback token Secret for clients that cannot send an Authorization header. |
| mcp.token.existingSecret | string | `""` | Existing Secret holding the fallback token. Empty disables the fallback. |
| mcp.token.key | string | `"token"` | Key in that Secret holding the token. |
| mcp.service.type | string | `"ClusterIP"` | Service type for the MCP endpoint. |
| mcp.service.port | int | `80` | Service port for the MCP endpoint (container port 8080). |
| mcp.service.metricsPort | int | `8081` | Service port exposing MCP metrics (container port 8081), separate from MCP traffic. |
| mcp.service.annotations | object | `{}` | Annotations to add to the MCP Service. |
| mcp.service.trafficDistribution | string | `""` | Traffic distribution preference (e.g. PreferClose). Empty omits the field. |
| mcp.gateway.enabled | bool | `false` | Enable HTTPRoute for the MCP endpoint. |
| mcp.gateway.name | string | `""` | MCP HTTPRoute name. Defaults to <fullname>-mcp when empty. |
| mcp.gateway.parentRefs | list | `[{"group":"gateway.networking.k8s.io","kind":"Gateway","name":"main-gateway","namespace":"gateway-system","sectionName":"https"}]` | Parent Gateway references for the MCP route. |
| mcp.gateway.hostnames | list | `["forklift-mcp.example.com"]` | Hostnames for the MCP route. |
| mcp.gateway.rules | list | `[{"backendRefs":[{"group":"","kind":"Service","name":"","port":"","weight":1}],"filters":[],"matches":[{"path":{"type":"PathPrefix","value":"/"}}]}]` | MCP HTTP route rules. |
| mcp.gateway.rules[0].filters | list | `[]` | HTTPRoute filters. |
| mcp.gateway.rules[0].backendRefs | list | `[{"group":"","kind":"Service","name":"","port":"","weight":1}]` | HTTPBackendRefs. Empty name/port default to the MCP Service and mcp.service.port. |
| mcp.serviceMonitor.enabled | bool | `false` | Create a Prometheus Operator ServiceMonitor for the MCP server. |
| mcp.serviceMonitor.interval | string | `"30s"` | Scrape interval. |
| mcp.serviceMonitor.scrapeTimeout | string | `""` | Scrape timeout. Uses the Prometheus default when empty. |
| mcp.serviceMonitor.additionalLabels | object | `{}` | Additional labels for the MCP ServiceMonitor (e.g. release selector). |
| mcp.resources | object | `{"limits":{"memory":"64Mi"},"requests":{"cpu":"10m","memory":"32Mi"}}` | Resource requests and limits for the MCP container. |
| mcp.resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | In-place vertical scaling policy: CPU in place, restart on memory resize. |
| mcp.podAnnotations | object | `{}` | Annotations to add to MCP pods. |
| mcp.podLabels | object | `{}` | Labels to add to MCP pods. |
| mcp.nodeSelector | object | `{}` | Node selector for MCP pod scheduling. |
| mcp.tolerations | list | `[]` | Tolerations for MCP pod scheduling. |
| mcp.affinity | object | `{}` | Affinity rules for MCP pod scheduling. |
| mcp.priorityClassName | string | `""` | Priority class name for MCP pods. Empty uses the cluster default. |
| mcp.extraEnv | list | `[]` | Raw environment variables appended to the MCP container. |
| serviceMonitor.enabled | bool | `false` | Create a Prometheus Operator ServiceMonitor. |
| serviceMonitor.interval | string | `"30s"` | Scrape interval. |
| serviceMonitor.scrapeTimeout | string | `""` | Scrape timeout. Uses the Prometheus default when empty. |
| serviceMonitor.additionalLabels | object | `{}` | Additional labels for the ServiceMonitor (e.g. release selector). |
| nodeSelector | object | `{}` | Node selector for pod scheduling. |
| tolerations | list | `[]` | Tolerations for pod scheduling. |
| affinity | object | `{}` | Affinity rules for pod scheduling. |
| topologySpreadConstraints | list | `[]` | Topology spread constraints for pod scheduling. |
| priorityClassName | string | `""` | Priority class name for pod scheduling. Empty uses the cluster default. |
| dnsPolicy | string | `""` | Pod DNS policy. Empty uses the cluster default. |
| dnsConfig | object | `{}` | Pod DNS configuration. Required when dnsPolicy is None. |
| extraEnv | list | `[]` | Raw environment variables appended to the container. |
| extraArgs | list | `[]` | Raw CLI flags appended to the container args (after the scan flags). |
| extraObjects | list | `[]` | Arbitrary additional manifests to render (each value is templated). |

## Source Code

* <https://github.com/younsl/addons/tree/main/box/kubernetes/forklift/charts/forklift>

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| younsl | <cysl@kakao.com> | <https://github.com/younsl> |

## License

This chart is licensed under the Apache License 2.0. See [LICENSE](https://github.com/younsl/addons/blob/main/LICENSE) for details.

## Contributing

This repository does not accept external contributions. Pull requests and issues are disabled.

----------------------------------------------
Autogenerated from chart metadata using [helm-docs v1.14.2](https://github.com/norwoodj/helm-docs/releases/v1.14.2)
