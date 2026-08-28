# Volume-free storage

Status: implemented in app 1.7.0 / chart 0.11.0

The chart provisions a 1Gi RWO PVC and mounts it at `/data` in both the `scraper` and the `server` pod. Neither pod treats it as storage. The scraper opens `/data/trivy.db` as a writer, the server opens the same file as a reader, and the PVC exists only so that two processes can point at one SQLite file. It is an IPC channel wearing a PersistentVolumeClaim.

Everything painful about the current deployment follows from that. The server cannot run more than one replica, because a second replica would need the same RWO volume on the same node. Both pods are pinned to whichever node and availability zone the EBS volume was created in. The scraper is fixed at `replicas: 1` with `strategy: Recreate`, so every scraper rollout is a full outage of the write path, and the comment in `deployment-scraper.yaml` explaining the single writer is really explaining the volume. Node loss means waiting on a CSI detach and reattach before either pod is schedulable again.

This design removes the PersistentVolume entirely. No pod in the chart mounts a volume that survives its own lifetime. The state that genuinely cannot be regenerated moves into Kubernetes objects, and the state that can be regenerated moves into an `emptyDir` owned by exactly one process.

## What the volume actually holds

Every table on the PVC falls into one of three durability classes, and only one of them needs anything durable at all.

| State | Class | Origin | Volume-free home |
| --- | --- | --- | --- |
| `reports` (counts, `data` blob) | derived | `VulnerabilityReport` / `SbomReport` CRs in each watched cluster | scraper `emptyDir` SQLite |
| `reports.notes*` | authored | a human typing in the UI | ConfigMap |
| `api_tokens` | authored | a human minting a token | Secret |
| `api_logs` | observational | the server's own request handler | stdout |
| `cleanup_history` | observational | the retention job, which stops existing | removed |
| alert rules | authored | a human, already ConfigMap-backed in `alerts/store.rs` | unchanged |
| sessions | derived | the OIDC flow, already an encrypted cookie | unchanged |

The `reports` table is 99% of the bytes and 0% of the irreplaceable data. Measured on a production hub cluster: 433MB of database holding 2078 reports (1554 sbom, 524 vuln) at roughly 208KB per report, against 3790 `api_logs` rows and a handful of notes and tokens. The expensive class is the regenerable one.

## Reports are a mirror, not a record

The scraper does not author reports. `LocalWatcher` in `src/web/watcher.rs` runs a `kube::runtime::watcher` per cluster per report type, and the SQLite rows are a projection of the CRs that exist right now. `Event::Apply` and `Event::InitApply` upsert, `Event::Delete` deletes. The authoritative copy lives in each watched cluster's API server, and trivy-operator rewrites it on its own schedule.

A `kube` watcher's stream begins with `Event::Init`, a full paginated list, and `Event::InitDone`. A scraper starting against an empty database therefore rebuilds the complete report set from the source of truth with no extra code and no extra API calls beyond the ones it already makes on every restart today. The PVC is not protecting the reports. It is caching them across restarts, and paying for that cache with the entire set of scheduling constraints above.

The cache also actively drifts. The watcher prunes nothing at `InitDone`, so a CR deleted while the scraper is down leaves its row behind forever. That is one of the two reasons the database grew from 357MB to 433MB over a 30 day window in which the report count barely moved from 1979 to 2078. The other is the absence of `VACUUM`. An `emptyDir` that starts empty on every restart eliminates both by construction, and with it the projected PVC saturation roughly seven months out.

## Target architecture

The scraper becomes the only process that opens a database. The server becomes a stateless HTTP tier in front of it.

```
scraper pod (replicas 1, RollingUpdate is now possible)
  watchers (local + one per registered edge cluster)
    -> SQLite on emptyDir /data/trivy.db          sole writer, sole reader
  alert evaluator                                 co-located with the write path
  :8081 /internal/v1/*                            read API, shared-token auth
  :9090 /healthz /readyz /metrics

server pod (replicas N, no volume, no database)
  UI, /api/v1/*, /mcp, OIDC, RBAC
    -> RemoteStore -> http://<scraper>:8081/internal/v1/*
  token store    -> Secret, watched into memory
  notes store    -> ConfigMap, watched into memory
  alert rule CRUD-> ConfigMap
  request log    -> stdout
```

The split follows data ownership rather than read and write roles. The writer owns the file, and nothing else can see it. That is what makes the server disposable, and a disposable server is the point: image bumps, config changes, and replica changes on the UI tier are frequent, while scraper restarts are rare and self-healing.

## reports: emptyDir under the scraper

`STORAGE_PATH` stays `/data` and `Database::new` is unchanged. Only the volume behind the mount changes, from a PVC to an `emptyDir` with a `sizeLimit`.

An `emptyDir` draws from the node's ephemeral storage, so the scraper must declare it. With node allocatable ephemeral storage around 25.9GiB and a 433MB database that WAL and rebuild churn can briefly double, `sizeLimit: 2Gi` on the volume plus a matching `ephemeral-storage` request and limit on the container gives the kubelet what it needs to schedule honestly and to evict this pod rather than a neighbour if the estimate is wrong.

The scraper also starts serving queries, including `search_sbom_components` and `search_vulnerabilities`, which scan the `data` column. Its memory limit has to grow to cover SQLite page cache and result construction, and the server's can shrink by roughly the same amount, since the server stops holding a connection pool and stops materialising query results from a local file.

## Hydration is now a visible state

Between scraper start and the last `InitDone` across all watched clusters, the database is legitimately incomplete. Today the PVC hides this. Without it, an empty dashboard right after a scraper restart would be indistinguishable from a real answer, which is worse than a brief error.

`WatcherStatus` already tracks `vuln_initial_sync_done` and `sbom_initial_sync_done`, but only as two global booleans, so a fleet of edge clusters cannot be represented. It becomes a per-cluster map of the two flags, published on `/internal/v1/hydration`. The server surfaces it three ways: `/readyz` on the scraper stays not-ready until every registered cluster reports done, the UI renders a rebuilding banner instead of empty tables, and MCP query tools return an explicit retryable error rather than a confidently empty result set. Alert evaluation stays suppressed until hydration completes, otherwise a rebuild would re-fire every finding in the fleet as net-new.

## api_tokens: a Secret with a watch cache

Tokens are authored, hashed, and unrecoverable, so they need a durable home. One Secret, `{fullname}-api-tokens`, holds them all, mirroring how `alerts/store.rs` already keeps every alert rule in one ConfigMap.

The data key is the token prefix that `create_token` already computes, `tc_` plus 8 hex characters, which is a valid Secret key as it stands. The value is the JSON of what the row holds minus the prefix: `user_sub`, `name`, `description`, `token_hash`, `created_at`, `expires_at`, `groups`. Validation keeps its current shape. Take the first 11 characters of the presented Bearer token, look up that one key, then compare the SHA-256 hash in constant time. No scan, no change to the hashing scheme, and the plaintext still never leaves the response that created it.

Two details decide whether this is workable. First, an API server GET per authenticated request is not acceptable, so the server watches the Secret into an in-memory map, the same pattern `hub/secret_watcher.rs` uses for cluster registrations. Revocation then propagates at watch latency instead of instantly, which is the correct tradeoff for a token that already carries an expiry. Second, `last_used_at` is a write on every request. It becomes a best-effort coalesced update, flushed at most once every few minutes per token, and it is explicitly allowed to be lost on pod exit.

The namespace `Role` already grants get, list, watch, create, update, patch, and delete on secrets and configmaps, so this adds no RBAC surface.

## notes: a ConfigMap joined at read time

Notes are the one piece of report state a human types, and they are the reason the reports table cannot simply be declared disposable. They move to `{fullname}-notes`.

Report identity is `(cluster, report_type, namespace, name)`, which can contain characters a ConfigMap key rejects, so the key is the SHA-256 hex of that tuple and the value is JSON carrying the tuple back along with `notes`, `notes_created_at`, and `notes_updated_at`. Nothing in the query layer filters or sorts on notes, they are only projected into list and detail responses, so the server can watch the ConfigMap into memory and merge notes into responses after the proxy call returns. The three `notes*` columns drop out of the schema and the scraper never sees them.

A ConfigMap caps at roughly 1MiB total. That is ample for the current volume but it is a hard wall rather than a soft one, so the write path rejects a single note over 8KiB, refuses a write that would push the object past 800KiB with a clear error rather than a truncated object, and exports the current size as a metric.

## api_logs: stdout, and the admin page goes away

The `api_logs` table is written by `web/logging_middleware.rs`, trimmed by a background task in `web.rs`, and read by the admin API logs page. Keeping it would mean either an HTTP write from the server to the scraper on every request or a database in the stateless tier, and both are worse than the feature.

Requests become one structured JSON line each on stdout, which the cluster's log pipeline already collects and can already query. `list_api_logs`, `get_api_log_stats`, `cleanup_old_api_logs`, and `count_api_logs` are deleted along with the table, the retention task, the `trivy_collector_api_logs` metric, and the admin page that reads them. This is a deliberate feature removal and the only user-visible regression in this design.

`cleanup_history` and the reports retention job go with it, for a different reason. Retention over a mirror is meaningless. The set of reports is defined by the CRs that exist, the watcher removes rows when they are deleted, and a rebuild drops anything stale.

## alerts: the evaluator moves to the scraper

Alert evaluation is triggered from exactly one place, the `receive_report` handler in `src/web/handlers.rs`, on the HTTP ingest path. The current scraper writes straight to the shared file and never calls that endpoint, so in the deployed topology the evaluator is unreachable and alerts do not fire at all. This design is the occasion to fix that, because it forces the question of where writes happen.

The evaluator moves into the scraper and hangs off the watcher's upsert path, where the previous `data_json` needed for net-new diffing is already in hand. Rule CRUD, preview, and test delivery stay on the server against the same ConfigMap, since both pods already have a Kubernetes client and `AlertStore` is safe to read from two places. Preview and test queries reach the data through the proxy like any other read.

The external `POST /api/v1/reports` route stays on the server as a thin proxy to the scraper's internal ingest, so any remaining pusher keeps working and its writes go through the same alert-evaluating path as a watcher event.

## The internal API

The scraper exposes a versioned read API on a new port, mirroring the `Database` methods the server actually calls rather than inventing a second query language. Response bodies are the existing `serde` models from `storage/models.rs` and `web/types.rs`.

| Route | Backing method |
| --- | --- |
| `GET /internal/v1/reports` | `query_reports` |
| `GET /internal/v1/reports/{cluster}/{type}/{namespace}/{name}` | `get_report` |
| `GET /internal/v1/stats` | `get_stats` |
| `GET /internal/v1/clusters` | `list_clusters` |
| `GET /internal/v1/namespaces` | `list_namespaces` |
| `GET /internal/v1/search/vulnerabilities` | `search_vulnerabilities` |
| `GET /internal/v1/search/components` | `search_sbom_components` |
| `GET /internal/v1/suggest/vulnerability-ids` | `suggest_vulnerability_ids` |
| `GET /internal/v1/suggest/component-names` | `suggest_component_names` |
| `GET /internal/v1/dashboard/trends` | `get_live_trends`, `get_reports_data_range` |
| `GET /internal/v1/hydration` | per-cluster `WatcherStatus` |
| `POST /internal/v1/reports` | `upsert_report` plus alert evaluation |
| `DELETE /internal/v1/reports/{cluster}/{type}/{namespace}/{name}` | `delete_report` |
| `DELETE /internal/v1/clusters/{cluster}` | `delete_reports_for_cluster` |

This API returns every report in the fleet with no per-user filtering, because RBAC is applied above it in the server. Reachable unauthenticated from anywhere in the namespace it would be a straight downgrade from today, where the data sits behind a filesystem permission. It is protected two ways: a shared token generated into a Secret at install time, mounted into both pods and compared in constant time on every internal request, and a NetworkPolicy admitting only the server pods on that port. The port is published through a ClusterIP Service and is never added to the Ingress, HTTPRoute, or any ServiceMonitor.

## Code changes

The read paths currently take `&Database` directly, in `web/handlers.rs`, `web/admin_handlers.rs`, `web/cluster_handlers.rs`, `mcp/handler.rs`, and `alerts/`. They move behind a `ReportStore` trait in `src/storage/`, with two implementations: `Database` as it exists, used by the scraper, and a new `RemoteStore` built on the `reqwest` client already in the dependency tree, used by the server. `AppState` holds an `Arc<dyn ReportStore>`.

Trait objects and `async fn` in traits do not mix on stable Rust, so this needs `async-trait`, one new dependency. The alternative, making `AppState` generic over the store, would push a type parameter through every axum handler signature and is not worth it.

New files: `src/storage/store.rs` for the trait, `src/storage/remote.rs` for the HTTP implementation, `src/collector/api.rs` for the scraper's internal server, `src/storage/notes.rs` and `src/storage/token_store.rs` for the ConfigMap and Secret backed stores. Deleted: `src/storage/api_logs.rs`, and the `api_logs`, `cleanup_history`, and `notes*` pieces of `schema.rs`.

Chart changes: `templates/pvc.yaml` is deleted, `server.persistence` is removed from `values.yaml`, both deployments switch their `data` volume to `emptyDir` (the server's disappears entirely along with the mount), the scraper gains `ephemeral-storage` requests and limits, the scraper's `strategy: Recreate` and hardcoded `replicas: 1` comment change meaning (a single writer is now a choice about watch load, not about a volume), the server gains a working `replicaCount` and a PodDisruptionBudget, and new templates appear for the internal Service, the shared-token Secret, and the NetworkPolicy.

## Alternatives considered

**Server owns the emptyDir, scraper pushes over HTTP.** The cheapest option by a wide margin. `collector/sender.rs` and the `receive_report` handler are both still in the tree from the legacy edge-push architecture, so the plumbing exists, every read path keeps its local `Database` and needs no trait, and alerts start firing again for free. It was rejected because it puts the state in the tier that restarts most. Every server image bump, config change, or replica change would discard the database and require the scraper to relist the entire fleet, and the server would still be stuck at one replica. It optimises the migration at the cost of the property the migration is for.

**One pod running both roles.** Removing the split removes the IPC problem outright, since a single process needs no shared file. Rejected because it merges a watch loop against N clusters with a user-facing HTTP tier into one blast radius and one resource envelope, which is the arrangement the current two-deployment split was created to escape. It also fixes the UI tier at one replica permanently.

**Reports in CRs or etcd.** Rejected on measurement. 433MB is 21% of the default 2GiB etcd quota on a managed control plane, a single SbomReport can exceed the 1.5MiB `max-request-bytes` limit, and the API server offers none of the multi-field filtering, aggregation, or JSON scanning that the dashboard and the SBOM component search are built on.

**Everything in memory, no file at all.** Rejected because 433MB of database exceeds the scraper's memory limit, and raising a memory limit to hold what a disk holds trades a cheap resource for an expensive one.

## Cutover

The existing PVC holds real tokens and real notes, and both are unrecoverable, so this is not a redeploy. A one-shot `export-state` subcommand reads a database path and writes the Secret and the ConfigMap through the API server. It runs as a Job that mounts the existing PVC read-only, after which the new chart version can be installed and the PVC deleted. Reports need no export, because the next scraper start relists them.

Order matters. Export first and verify both objects, then upgrade, then confirm hydration completes and that a known token still authenticates and a known note still renders, and only then delete the PVC. The PVC is the rollback.

## Validation

- Tokens minted before the cutover still authenticate afterwards, and a revoked token stops working within watch latency.
- A note written before the cutover still renders on its report after a full scraper restart, which is the case the old design could not survive without a volume.
- `kubectl delete pod` on the scraper leads to a complete report set with no manual step, and `/readyz` stays false for the whole rebuild.
- The server scales to three replicas and rolls with no disruption to the UI.
- No `PersistentVolumeClaim` exists in the release, and `kubectl get pvc` in the namespace is empty.
- The internal port refuses a request with no token and is unreachable from a pod that is not the server.
- Line coverage stays at or above 70% under `cargo llvm-cov`, with the new `RemoteStore`, notes store, and token store covered.
