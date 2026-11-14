# oxynote

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.10.0](https://img.shields.io/badge/AppVersion-0.10.0-informational?style=flat-square)

A Helm chart for Oxynote, an open-source technical wiki connected to the systems it describes

**Homepage:** <https://github.com/oxynote/oxynote>

## Requirements

Kubernetes: `>=1.29.0-0`

## Installation

### List available versions

This chart is distributed via OCI registry, so you need to use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) instead of `helm search repo` to discover available versions:

```console
crane ls ghcr.io/younsl/charts/oxynote
```

If you need to install crane on macOS, you can easily install it using [Homebrew](https://brew.sh/), the package manager.

```bash
brew install crane
```

### Install the chart

Install the chart with the release name `oxynote`:

```console
helm install oxynote oci://ghcr.io/younsl/charts/oxynote
```

Install with custom values:

```console
helm install oxynote oci://ghcr.io/younsl/charts/oxynote -f values.yaml
```

Install a specific version:

```console
helm install oxynote oci://ghcr.io/younsl/charts/oxynote --version 0.1.0
```

### Install from local chart

Download oxynote chart and install from local directory:

```console
helm pull oci://ghcr.io/younsl/charts/oxynote --untar --version 0.1.0
helm install oxynote ./oxynote
```

The `--untar` option downloads and unpacks the chart files into a directory for easy viewing and editing.

## Upgrade

```console
helm upgrade oxynote oci://ghcr.io/younsl/charts/oxynote
```

## Uninstall

```console
helm uninstall oxynote
```

## Configuration

The following table lists the configurable parameters and their default values.

## Values

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| nameOverride | string | `""` | Override the chart name used in resource names |
| fullnameOverride | string | `""` | Override the full resource name |
| image.registry | string | `"ghcr.io"` | Image registry. Empty renders the repository without a registry prefix |
| image.repository | string | `"oxynote/oxynote"` | Image repository under the registry. The image is published for linux/amd64 and linux/arm64 |
| image.tag | string | `""` | Image tag. Defaults to the chart appVersion |
| image.digest | string | `""` | Image digest, for example `sha256:...`. Takes precedence over `tag` when set |
| image.pullPolicy | string | `"IfNotPresent"` | Image pull policy |
| imagePullSecrets | list | `[]` | Image pull secrets for private registries or pull-through caches |
| oxynote.publicUrl | string | `""` | Origin users open in the browser, for example `https://notes.example.com` (sets `OXYNOTE_PUBLIC_URL`). Scheme and host only. Empty keeps the upstream default `http://localhost:8080` |
| oxynote.config | object | `{}` | Plain `OXYNOTE_*` environment variables rendered as-is. Keys outside the upstream list fail the boot, so typos surface at startup. See https://github.com/oxynote/oxynote/blob/main/docker/prod/README.md |
| oxynote.existingSecret | string | `""` | Name of an existing Secret whose keys are loaded as environment variables. Put credentials here, such as `OXYNOTE_DB_DSN`, `OXYNOTE_SMTP_DSN`, `OXYNOTE_OBJECT_STORAGE_DSN`, `OXYNOTE_AI_ASSISTANT_API_KEY`. Every key must be a valid `OXYNOTE_*` name |
| githubApp.privateKeySecret | string | `""` | Name of an existing Secret holding the GitHub App private key, mounted at `/oxynote/github/private-key.pem`. Also set `OXYNOTE_GITHUB_APP_ID`, `OXYNOTE_GITHUB_APP_SLUG` and `OXYNOTE_GITHUB_APP_SIGNATURE_SECRET` |
| githubApp.privateKeySecretKey | string | `"private-key.pem"` | Key inside `githubApp.privateKeySecret` that holds the PEM file |
| persistence.enabled | bool | `true` | Persist `/oxynote/data` (embedded PostgreSQL, generated secrets, uploaded images, search index) on a PersistentVolumeClaim. Disabled uses an emptyDir, which loses every document on pod restart |
| persistence.existingClaim | string | `""` | Use an existing PersistentVolumeClaim instead of creating one |
| persistence.storageClassName | string | `""` | StorageClass for the created claim. Empty uses the cluster default |
| persistence.accessModes | list | `["ReadWriteOnce"]` | Access modes for the created claim |
| persistence.size | string | `"10Gi"` | Size of the created claim |
| persistence.annotations | object | `{}` | Annotations for the created claim |
| persistence.keep | bool | `true` | Keep the created claim on `helm uninstall` (`helm.sh/resource-policy: keep`). Losing the volume signs everyone out and orphans stored data-source credentials |
| serviceAccount.create | bool | `true` | Create a ServiceAccount |
| serviceAccount.name | string | `""` | ServiceAccount name. Defaults to the full name when `create` is true, `default` otherwise |
| serviceAccount.annotations | object | `{}` | ServiceAccount annotations |
| serviceAccount.automountServiceAccountToken | bool | `false` | Mount the ServiceAccount token. Oxynote does not call the Kubernetes API |
| service.type | string | `"ClusterIP"` | Service type |
| service.port | int | `8080` | Service port that forwards to the container port 8080 |
| service.annotations | object | `{}` | Service annotations |
| ingress.enabled | bool | `false` | Create an Ingress. The Ingress API is frozen, so prefer `httpRoute` for new installs |
| ingress.className | string | `""` | IngressClass name |
| ingress.annotations | object | `{}` | Ingress annotations. Allow request bodies of at least 30MB for uploads |
| ingress.hosts | list | `[{"host":"notes.example.com","paths":[{"path":"/","pathType":"Prefix"}]}]` | Ingress hosts and paths |
| ingress.tls | list | `[]` | Ingress TLS configuration |
| httpRoute.enabled | bool | `false` | Create a Gateway API HTTPRoute |
| httpRoute.annotations | object | `{}` | HTTPRoute annotations |
| httpRoute.parentRefs | list | `[]` | Gateways this route attaches to |
| httpRoute.hostnames | list | `[]` | Hostnames matched by the route |
| httpRoute.matches | list | `[{"path":{"type":"PathPrefix","value":"/"}}]` | Route matches. The backend is always the chart Service |
| deploymentAnnotations | object | `{}` | Annotations added to the Deployment |
| podAnnotations | object | `{}` | Annotations added to the pod |
| podLabels | object | `{}` | Labels added to the pod |
| podSecurityContext | object | `{"fsGroup":65532,"fsGroupChangePolicy":"OnRootMismatch","runAsGroup":65532,"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}}` | Pod security context. UID 65532 is fixed by the upstream image. `OnRootMismatch` stops kubelet from adding group write to the PostgreSQL data directory on every mount, which PostgreSQL refuses |
| securityContext | object | `{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true}` | Container security context. The image writes only to `/oxynote/data` and `/tmp`, so the root filesystem stays read-only |
| resources | object | `{}` | Container resource requests and limits |
| resizePolicy | list | `[{"resourceName":"cpu","restartPolicy":"NotRequired"},{"resourceName":"memory","restartPolicy":"RestartContainer"}]` | In-place resize policy per resource. CPU resizes without a restart. Memory restarts the container, since the Node.js heap and PostgreSQL buffers are sized at startup |
| podDisruptionBudget.enabled | bool | `false` | Create a PodDisruptionBudget. With a single replica, `maxUnavailable: 0` or `minAvailable: 1` blocks node drains until the PDB is removed |
| podDisruptionBudget.minAvailable | int or string | `""` | Minimum available pods. Takes precedence over `maxUnavailable` when set |
| podDisruptionBudget.maxUnavailable | int or string | `1` | Maximum unavailable pods |
| podDisruptionBudget.unhealthyPodEvictionPolicy | string | `""` | Eviction policy for unhealthy pods (`IfHealthyBudget` or `AlwaysAllow`). Empty uses the cluster default |
| terminationGracePeriodSeconds | int | `60` | Grace period for the ordered shutdown that flushes open documents last |
| startupProbe | object | `{"failureThreshold":36,"httpGet":{"path":"/auth-realtime/api/auth-config","port":"http"},"periodSeconds":5,"timeoutSeconds":5}` | Startup probe. The upstream image allows 180s before health checks count |
| livenessProbe | object | `{"exec":{"command":["/usr/bin/node","/oxynote/prod/healthcheck.mjs"]},"failureThreshold":5,"periodSeconds":30,"timeoutSeconds":10}` | Liveness probe. Runs the image's layered health check, which fails when any internal service hangs |
| readinessProbe | object | `{"failureThreshold":3,"httpGet":{"path":"/auth-realtime/api/auth-config","port":"http"},"periodSeconds":10,"timeoutSeconds":5}` | Readiness probe through the front door |
| dnsPolicy | string | `"ClusterFirst"` | Pod DNS policy. `None` requires `dnsConfig.nameservers` |
| dnsConfig | object | `{}` | Pod DNS config |
| priorityClassName | string | `""` | PriorityClass name |
| nodeSelector | object | `{}` | Node selector |
| tolerations | list | `[]` | Tolerations |
| affinity | object | `{}` | Affinity rules |
| extraEnv | list | `[]` | Extra environment variables for the container |
| extraEnvFrom | list | `[]` | Extra envFrom sources for the container |
| extraVolumes | list | `[]` | Extra volumes for the pod |
| extraVolumeMounts | list | `[]` | Extra volume mounts for the container |

## Source Code

* <https://github.com/younsl/ct/tree/main/box/addons/charts/oxynote>
* <https://github.com/oxynote/oxynote>

## Maintainers

| Name | Email | Url |
| ---- | ------ | --- |
| younsl | <cysl@kakao.com> | <https://github.com/younsl> |

## License

This chart is licensed under the Apache License 2.0. See the [LICENSE](https://github.com/younsl/ct/blob/main/LICENSE) file for details.

## Contributing

This repository does not accept external contributions. Pull requests and issues are disabled.

----------------------------------------------
Autogenerated from chart metadata using [helm-docs v1.14.2](https://github.com/norwoodj/helm-docs/releases/v1.14.2)
