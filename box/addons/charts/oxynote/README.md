# oxynote

![Version: 0.1.0](https://img.shields.io/badge/Version-0.1.0-informational?style=flat-square) ![Type: application](https://img.shields.io/badge/Type-application-informational?style=flat-square) ![AppVersion: 0.10.0](https://img.shields.io/badge/AppVersion-0.10.0-informational?style=flat-square)

A Helm chart for Oxynote, an open-source technical wiki connected to the systems it describes

**Homepage:** <https://github.com/oxynote/oxynote>

This chart is maintained by younsl and is not an official Oxynote project. The upstream image reference lives in [docker/prod/README.md](https://github.com/oxynote/oxynote/blob/main/docker/prod/README.md).

## Installation

The chart is distributed only as an OCI artifact on ghcr.io, so `helm search repo` does not work. List versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md):

```console
crane ls ghcr.io/younsl/charts/oxynote
```

Install:

```console
helm install oxynote oci://ghcr.io/younsl/charts/oxynote \
  --version 0.1.0 \
  --namespace oxynote --create-namespace \
  --set oxynote.publicUrl=https://notes.example.com
```

Inspect the chart before installing:

```console
helm show values oci://ghcr.io/younsl/charts/oxynote --version 0.1.0
```

## Design notes

- Single instance only. Oxynote keeps its search index on the data volume, so the Deployment runs one replica with the `Recreate` strategy and the replica count is not configurable.
- `enableServiceLinks` is always `false`. The image rejects any unknown `OXYNOTE_*` variable at boot, and Kubernetes injects `OXYNOTE_SERVICE_HOST` and friends when the Service is named `oxynote`.
- `/oxynote/data` holds the embedded PostgreSQL, the generated secrets, uploaded images and the search index. Losing it loses every document and permanently orphans stored data-source credentials, so the created PVC is kept on `helm uninstall` by default.
- `fsGroupChangePolicy: OnRootMismatch` is the default because a recursive `fsGroup` change adds group write to the PostgreSQL data directory, which PostgreSQL refuses to open.
- The root filesystem is read-only and `/tmp` is an emptyDir. The embedded PostgreSQL socket lives there.

## Configuration

Set the public origin with `oxynote.publicUrl`. Put plain settings in `oxynote.config` and credentials in a Secret referenced by `oxynote.existingSecret`:

```yaml
oxynote:
  publicUrl: https://notes.example.com
  config:
    OXYNOTE_LOG_LEVEL: INFO
    OXYNOTE_CRASH_REPORTING_DISABLED: "true"
  existingSecret: oxynote-env
```

```console
kubectl -n oxynote create secret generic oxynote-env \
  --from-literal=OXYNOTE_DB_DSN='postgresql://oxynote:${DB_PASSWORD}@db.example.com/oxynote?sslmode=require' \
  --from-literal=OXYNOTE_SMTP_DSN='smtp://${SMTP_USER}:${SMTP_PASSWORD}@smtp.example.com:587?tls=starttls'
```

The default single-workspace mode creates the admin `admin@example.com` with the password `oxynote-admin-1234` on first boot. Change both before exposing the instance.

## Backup

With the embedded database, dump it with the `pg_dump` the image ships:

```console
kubectl -n oxynote exec deploy/oxynote -- sh -c 'PGPASSWORD="$(cat /oxynote/data/secrets/database-password)" /usr/libexec/postgresql18/pg_dump -h /tmp/postgresql -U oxynote oxynote' > oxynote.sql
```

## Requirements

Kubernetes: `>=1.29.0-0`

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
