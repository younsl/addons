# Getting Started

## Prerequisites

- **Central cluster**: Kubernetes cluster where the chart is installed. [Trivy Operator](https://github.com/aquasecurity/trivy-operator) is required only if you want to scrape the central cluster itself (`scraper.watchLocal: true`).
- **Edge clusters**: Trivy Operator installed. Network reachability from the central cluster to each Edge API server (the scraper pod initiates all connections).

## 1. Install on the central (Hub) cluster

The recommended installation method is the [Helm chart](https://helm.sh/docs/topics/charts/), distributed as an [OCI artifact](https://helm.sh/docs/topics/registries/) on GitHub Container Registry. The same command installs the release and upgrades it later.

```bash
helm upgrade --install trivy-collector oci://ghcr.io/younsl/charts/trivy-collector \
  --namespace trivy-system \
  --create-namespace \
  --set server.gateway.enabled=true \
  --set server.gateway.hostnames[0]=trivy.example.com
```

The UI is exposed through a Gateway API `HTTPRoute`. Plain `Ingress` is not supported.

This creates **two [Deployments](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/)** and no PersistentVolumeClaim:

- `trivy-collector-server` (UI on port 3000, no database, scale it freely)
- `trivy-collector-scraper` (one replica, owns all watchers and the database)

The scraper keeps SQLite on its own `emptyDir` and serves it back to the server pods over an internal API on port 8081, guarded by a shared token and a NetworkPolicy. Report rows are a mirror of the CRs that exist right now, so an empty start costs one relist and nothing else. State that a human authored lives in Kubernetes objects instead: report notes in `{release}-notes` (ConfigMap) and API tokens in `{release}-api-tokens` (Secret).

Upgrading from a release that used a PVC? Export the authored state first, since tokens and notes are unrecoverable:

```bash
helm upgrade trivy-collector oci://ghcr.io/younsl/charts/trivy-collector \
  --namespace trivy-system \
  --set migration.exportState.enabled=true \
  --set migration.exportState.existingClaim=trivy-collector
```

Verify both objects, confirm a known token still authenticates and a known note still renders, and only then delete the old PVC. Until that last step the PVC is the rollback. See [Upgrading](upgrading.md) for every release's steps.

## 2. Register an Edge cluster via the UI

Open `/admin/clusters`, click **Create** (or go straight to `/admin/clusters/new`), and use the two-step wizard:

**Step 1 (Bootstrap)**: copy the generated YAML and apply it on the Edge cluster with an admin kubeconfig. It installs:

- `ServiceAccount: trivy-collector-reader`
- `ClusterRole` with `get / list / watch` on `aquasecurity.github.io` `vulnerabilityreports` and `sbomreports` only (no write, no wildcards)
- `ClusterRoleBinding`
- `Secret` of type `kubernetes.io/service-account-token` that populates a long-lived SA token

**Step 2 (Register)**: run the copy-paste bash block on the Edge cluster to extract the SA token, CA, and API server URL, paste them into the form, and click **Register cluster**. The scraper attaches within a few seconds and the table flips to **Synced**.

No collector pod is deployed on the Edge cluster, only the four RBAC resources above.

## 3. GitOps-based registration (optional)

Registration is a plain `Secret`, so it can be managed via Helm, ArgoCD ApplicationSet, SealedSecrets, etc. without touching the UI:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: cluster-edge-a-<api-host>   # or any DNS-1123 name
  namespace: trivy-system
  labels:
    trivy-collector.io/secret-type: cluster
type: Opaque
stringData:
  name: edge-a
  server: https://<edge-api-server>:443
  config: |
    {
      "bearerToken": "<decoded-SA-token>",
      "tlsClientConfig": { "caData": "<base64-CA>" }
    }
  namespaces: "[]"      # empty = watch all
```

## RBAC footprint

On each Edge cluster the registration installs a strictly read-only `ClusterRole`:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: trivy-collector-reader
rules:
  - apiGroups: ["aquasecurity.github.io"]
    resources: ["vulnerabilityreports", "sbomreports"]
    verbs: ["get", "list", "watch"]
```

On the central cluster the chart creates a namespaced `Role` for Secret CRUD (scoped to the release namespace) and a ClusterRole for reading Trivy CRDs on the central cluster itself. See [Architecture](architecture.md#hub-rbac-footprint) and, for the application's own access control, [RBAC](rbac.md).

## Conclusion

Install the chart once on the hub, then register each edge cluster through the UI wizard or a labelled `Secret` in Git. Edges receive only a read-only ServiceAccount and ClusterRole, and the scraper starts pulling their reports within seconds of registration.
