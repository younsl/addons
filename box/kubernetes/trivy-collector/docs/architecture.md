# Architecture

## Overview

trivy-collector uses an **ArgoCD-style hub-pull** architecture. All pods run on
the central (Hub) cluster; Edge clusters host only a small read-only
`ServiceAccount` that the Hub uses to watch Trivy Operator CRDs remotely.

![Architecture](assets/3-architecture.png)

```
              ┌─ Central (Hub) cluster ────────────────────────┐
              │                                                │
              │  trivy-collector-server   (--mode=server)      │
              │    ├─ HTTP UI / API on :3000                   │
              │    ├─ no database, no volume                   │
              │    └─ reads via scraper :8081 (internal API)   │
              │                                                │
              │  trivy-collector-scraper  (--mode=scraper)     │
              │    ├─ local Trivy watcher (Hub's own cluster)  │
              │    ├─ Secret watcher      (Hub namespace)      │
              │    │    └─ spawns per-cluster watchers         │
              │    ├─ per-cluster watchers (one per Edge)      │
              │    ├─ alert evaluator     (on the write path)  │
              │    ├─ SQLite on emptyDir  (sole reader/writer) │
              │    └─ internal read API on :8081              │
              │                                                │
              │  ConfigMap {release}-notes      (report notes) │
              │  Secret    {release}-api-tokens (API tokens)   │
              │                                                │
              └────────────┬──────────────────┬────────────────┘
                           │ kube-apiserver   │ kube-apiserver
                           ▼                  ▼
              ┌─ Edge cluster A ──┐  ┌─ Edge cluster B ──┐
              │ Trivy Operator    │  │ Trivy Operator    │
              │ SA (read-only)    │  │ SA (read-only)    │
              └───────────────────┘  └───────────────────┘
```

## Two pods, single responsibility

One Helm release creates two Deployments on the central cluster, distinguished
by the `--mode` CLI flag:

| Pod | Mode | Role |
|---|---|---|
| `trivy-collector-server` | `--mode=server` | HTTP UI + API. Holds no database and mounts no volume; reads reports through the scraper's internal API. No watchers. |
| `trivy-collector-scraper` | `--mode=scraper` | Runs all watchers, owns the only database, evaluates alert rules, and serves the internal read API on `:8081`. No UI (only `/healthz`, `/readyz`, `/metrics`). |

The split follows data ownership rather than read and write roles. The scraper owns the SQLite file on its own `emptyDir` and nothing else can see it, which is what makes the server disposable, and a disposable server is the point, since image bumps, config changes, and replica changes on the UI tier are frequent while scraper restarts are rare and self-healing.

The scraper runs one replica by choice, not by constraint: a second would double the watch load on every registered cluster for no benefit. Its rollout is a `RollingUpdate`, because the incoming pod reports unready until the fleet is hydrated and the outgoing pod keeps serving reads for the whole rebuild. The server scales horizontally.

## Where state lives

Every table that used to sit on the PersistentVolume falls into one of three durability classes, and only one of them needs anything durable.

| State | Class | Origin | Home |
|---|---|---|---|
| `reports` | derived | `VulnerabilityReport` / `SbomReport` CRs in each watched cluster | scraper `emptyDir` SQLite |
| report notes | authored | a human typing in the UI | ConfigMap `{release}-notes` |
| API tokens | authored | a human minting a token | Secret `{release}-api-tokens` |
| request logs | observational | the server's own request handler | stdout |
| alert rules | authored | a human | ConfigMap `{release}-alerts` |
| sessions | derived | the OIDC flow | encrypted cookie |

`reports` is almost all of the bytes and none of the irreplaceable data. Each watcher's stream begins with a full paginated list, so a scraper starting against an empty database rebuilds the complete report set from the source of truth with no extra code and no extra API calls beyond the ones it already makes on every restart. Starting empty also prunes what the old cache accumulated: a CR deleted while the scraper was down used to leave its row behind forever.

## Hydration

Between scraper start and the last initial sync the report set is legitimately incomplete, and an empty dashboard right after a restart would otherwise be indistinguishable from a real answer. Hydration is tracked per cluster and per report type, and surfaced three ways:

- the scraper's `/readyz` stays failing until every registered cluster reports done, so the server is never routed to a partial set
- the UI renders a rebuilding banner instead of empty tables, from `GET /api/v1/hydration`
- alert evaluation stays suppressed until hydration completes, otherwise a rebuild would re-fire every finding in the fleet as net-new

## The internal API

The scraper exposes a versioned read API under `/internal/v1` on `:8081`, mirroring the storage methods the server actually calls rather than inventing a second query language. Response bodies are the same `serde` models the local store returns, so the two implementations cannot drift.

It answers with every report in the fleet and no per-user filtering, because RBAC is applied above it in the server. Reachable unauthenticated it would be a straight downgrade from the filesystem permission that used to protect the database, so it is fenced two ways: a shared token compared in constant time on every request, and a NetworkPolicy admitting only the server pods. The port is never added to the HTTPRoute or either ServiceMonitor.

The scraper in turn runs three kinds of watchers:

1. **Local watcher**: watches Trivy CRDs on the Hub's own cluster via the pod's
   own in-cluster ServiceAccount (no Secret needed). Toggled with
   `scraper.watchLocal`.
2. **Secret watcher**: watches `Secret` resources in the Hub namespace
   labelled `trivy-collector.io/secret-type=cluster`. On `Apply` it spawns a
   per-cluster watcher; on `Delete` it stops one.
3. **Per-cluster watchers**: one per registered Edge cluster. Each one holds a
   `kube::Client` built from the Secret's `bearerToken` + `caData` and watches
   the Edge's Trivy CRDs directly.

## Cluster registration (ArgoCD pattern)

A registered cluster is a plain Kubernetes `Secret` in the Hub namespace:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: cluster-edge-a-<api-host>
  namespace: trivy-system
  labels:
    trivy-collector.io/secret-type: cluster
    app.kubernetes.io/managed-by: trivy-collector
type: Opaque
stringData:
  name: edge-a
  server: https://<edge-api-server>:443
  config: |
    {
      "bearerToken": "<SA-token>",
      "tlsClientConfig": { "caData": "<base64-CA>" }
    }
  namespaces: "[]"   # empty = watch all
```

The schema is compatible with ArgoCD's own cluster Secrets. Secrets can be
created via:

- The Hub UI's two-step wizard (`/admin/clusters/new`)
- `POST /api/v1/hub/clusters` REST call
- `kubectl apply` / Helm / ArgoCD ApplicationSet / SealedSecrets (GitOps)

Credential model:

| Identity | Lifetime | Purpose |
|---|---|---|
| Operator's admin kubeconfig | seconds (bootstrap only) | Applies the read-only SA/Role/Binding/Token Secret on the Edge cluster |
| Edge SA token (`trivy-collector-reader`) | long-lived | Stored in the Hub Secret and used by the scraper to watch Trivy CRDs |

Only the long-lived SA token lands in the Hub Secret. The operator's admin
credentials never leave the operator's workstation.

## Data flow

```
Edge Trivy Operator
  creates VulnerabilityReport / SbomReport CRD
        │
        ▼
scraper's per-cluster watcher
  receives watch event
  tags report with Secret's `name` field (e.g. "edge-a")
  writes row to SQLite on its own emptyDir
  evaluates alert rules against the previous revision
        │
        ▼
scraper's internal API on :8081
  shared-token auth, NetworkPolicy-fenced
        │
        ▼
server proxies the read
  applies RBAC, joins notes from the ConfigMap
  renders Dashboard, Vulnerabilities, SBOM pages
```

Reports from the Hub's own cluster flow through the local watcher and are
tagged with `clusterName` (chart value).

## Single-pod vs two-pod rationale

Keeping watchers and the HTTP UI in separate processes makes several concerns
simpler:

- **Resource profile**: the scraper needs more memory (long-running watch
  streams, per-cluster kube clients, report JSON buffers) while the server is
  I/O light. Per-component `resources` blocks let the two be sized independently.
- **Scaling**: the server can run multiple replicas behind a Service for HA of
  the UI without risking duplicate DB writers.
- **Failure isolation**: a crash in a per-cluster watcher can't bring the UI
  down and vice versa.
- **Deployment auditability**: a single image with one of two CLI flags is
  easy to reason about (`--mode=server` vs `--mode=scraper`).

## Deletion semantics

Deleting a cluster via the UI or `DELETE /api/v1/hub/clusters/{name}` does
three things:

1. Deletes the Hub `Secret` → Secret watcher fires Delete → per-cluster watcher
   is cancelled
2. Deletes every row for that cluster from the `reports` table → it stops
   appearing in Dashboard / Vulnerabilities / SBOM views immediately
3. Leaves the Edge cluster's `trivy-collector-reader` RBAC intact (the operator
   can remove it manually later if desired)

## What is *not* deployed on Edge

- No `trivy-collector` pod
- No `Deployment`
- No HTTP server
- No Helm release

Only four Kubernetes resources, installed once:

1. `ServiceAccount: trivy-collector-reader`
2. `ClusterRole`: `get / list / watch` on `aquasecurity.github.io`
   `vulnerabilityreports` and `sbomreports` (no write, no wildcards)
3. `ClusterRoleBinding`
4. `Secret` of type `kubernetes.io/service-account-token` holding the long-
   lived token for the SA

All other logic lives on the central cluster.

## Registration flow

### Via UI (recommended)

`/admin/clusters/new` (the **Create** button on `/admin/clusters`) runs a two-step wizard:

1. **Bootstrap**: Copy the generated YAML (SA + ClusterRole +
   ClusterRoleBinding + token Secret) and `kubectl apply` on the Edge cluster
   with an admin kubeconfig. Then run the provided bash block to extract the
   SA token, CA, and API server URL.
2. **Register**: Paste the three extracted values into the form. Submitting
   calls `POST /api/v1/hub/clusters`, which creates the Hub Secret. The
   scraper attaches within seconds, the UI returns to `/admin/clusters`, and the
   table flips to **Synced**.

### Via GitOps / kubectl

Apply the Hub Secret directly:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: cluster-edge-a-<api-host>
  namespace: trivy-system
  labels:
    trivy-collector.io/secret-type: cluster
type: Opaque
stringData:
  name: edge-a
  server: https://edge-api:443
  config: |
    {
      "bearerToken": "<SA-token>",
      "tlsClientConfig": { "caData": "<base64-CA>", "insecure": false }
    }
  namespaces: "[]"   # empty = watch all; or '["default","prod"]' to filter
```

The scraper's Secret watcher picks it up within one watch event (typically
<1 s).

## HTTP API

| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/hub/clusters` | List registered clusters |
| POST | `/api/v1/hub/clusters` | Register or update a cluster |
| POST | `/api/v1/hub/clusters/validate` | Test credentials without saving |
| DELETE | `/api/v1/hub/clusters/{name}` | Unregister a cluster (purges its reports) |

All endpoints are protected by the standard auth/RBAC layer when
`AUTH_MODE=keycloak`.

## Configuration

Hub-pull is **always active** in scraper mode; there is no toggle. Cluster
Secrets are watched in the pod's own namespace (injected via Downward API
`fieldRef: metadata.namespace`); cross-namespace Secret watching is not
supported.

## Hub RBAC footprint

The chart creates three RBAC objects on the central cluster, bound to the
shared ServiceAccount:

| Object | Scope | Permissions |
|---|---|---|
| `ClusterRole` | cluster-wide | Read-only (`get / list / watch`) on `aquasecurity.github.io` `vulnerabilityreports` and `sbomreports`, used by the local watcher on the Hub's own cluster |
| `Role` | release namespace | `configmaps + secrets` `get / list / watch / create / update / patch / delete`, covers both alerts ConfigMap CRUD and cluster-registration Secret CRUD |
| `RoleBinding` | release namespace | Binds the above `Role` to the chart ServiceAccount |

The Role is deliberately namespaced to the release namespace to limit blast
radius if the Hub is ever compromised.

## Operational notes

- Add/remove a cluster takes effect within one Kubernetes watch event
  (typically <1 s).
- Each per-cluster watcher does an initial full list on start, then streams
  deltas. Initial sync latency scales with the number of reports on the Edge
  cluster.
- If an Edge cluster becomes unreachable, the watcher logs the error and keeps
  retrying. Other clusters are unaffected.
- SA tokens for Edge clusters are long-lived by default. For stricter
  rotation, rotate the Secret periodically, the scraper reconnects
  automatically when the Secret's `resourceVersion` changes.
- SQLite lives on the scraper's `emptyDir` in WAL mode, opened by that process
  alone. Because an `emptyDir` draws from the node's ephemeral storage, the
  scraper declares `ephemeral-storage` requests and limits alongside the
  volume's `sizeLimit` so the kubelet can schedule honestly and evict this pod
  rather than a neighbour if the estimate is wrong.
- Neither pod is pinned to a node or an availability zone any more, and node
  loss no longer waits on a CSI detach and reattach.
