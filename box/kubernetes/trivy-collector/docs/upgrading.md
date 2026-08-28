# Upgrading

## To app 1.7.0 / chart 0.12.0

This release removes the PersistentVolume and reworks the UI. The scraper now owns SQLite on its own `emptyDir` and serves it back to the server pods over an internal API, so the server holds no database and no volume. See [Architecture](architecture.md) for why.

It carries breaking changes under a minor version bump, so read this before upgrading.

The database size reported by `/api/v1/stats` and by the `trivy_collector_db_size_bytes` metric comes from the SQLite page pragmas rather than `fs::metadata` on the database file. The value is the logical size of the committed database, so it does not count a WAL segment that has not been checkpointed yet.

### Do this first: export the authored state

API tokens are hashed and report notes are typed by a human. Both are unrecoverable, and both used to live in the database on the PVC. Reports need no export because the next scraper start relists them from the clusters that own the CRs.

Dry run first to see what will move:

```bash
helm upgrade trivy-collector ./charts/trivy-collector \
  --namespace trivy-system \
  --set migration.exportState.enabled=true \
  --set migration.exportState.existingClaim=trivy-collector \
  --set migration.exportState.dryRun=true
```

Check the Job's logs for the token and note counts, then run it for real by dropping `dryRun`. The hook mounts the old PVC read-only and writes two objects:

- `Secret/{release}-api-tokens`
- `ConfigMap/{release}-notes`

Verify both exist, then confirm hydration completes, a known token still authenticates, and a known note still renders on its report. **Only then delete the PVC.** Until that step the PVC is the rollback.

### Removed Helm values

| Removed | Replacement |
| --- | --- |
| `server.persistence.*` | None. The release provisions no PVC. Scraper storage is `scraper.storage.*` (an `emptyDir`) |
| `server.ingress.*` | `server.gateway.*` (Gateway API `HTTPRoute`). Plain `Ingress` is no longer rendered; author one through `extraObjects` if you need it |

### New Helm values worth setting

| Value | Why |
| --- | --- |
| `internal.token` or `internal.existingSecret` | The shared token guarding the scraper's internal API. Left empty it is generated on first install and preserved across upgrades via `lookup`, which a GitOps controller rendering without cluster access **cannot** do. Set one explicitly under ArgoCD or Flux |
| `scraper.storage.sizeLimit` | Must cover the database plus WAL and rebuild churn, and stay in step with the container's `ephemeral-storage` limit |
| `server.replicaCount` | The UI tier is stateless now. Set `server.mcp.stateless=true` when raising it above 1 |
| `internal.networkPolicy.enabled` | Defaults on. Inert on a cluster whose CNI does not enforce policy |
| `scraper.collect.sbomReports` | SBOM reports are the bulk of the data. This flag now actually gates its watcher, so it is the main lever on database size |

### Removed API endpoints

Request logs are structured stdout lines now, collected and queried through the cluster's log pipeline. The table that backed them and the Admin Console page that read them are gone.

| Removed | Replacement |
| --- | --- |
| `GET /api/v1/admin/logs` | Cluster log pipeline (filter on `target=trivy_collector::access`) |
| `GET /api/v1/admin/logs/stats` | Same |
| `DELETE /api/v1/admin/logs` | None. Retention belongs to the log pipeline |
| UI route `/admin/audit` | None |

### Changed API endpoints

API tokens live in a Secret keyed by the token prefix, so there is no rowid to address them by.

| Before | After |
| --- | --- |
| `DELETE /api/v1/auth/tokens/{id}` | `DELETE /api/v1/auth/tokens/{prefix}` (e.g. `tc_ab12cd34`) |
| `TokenInfo.id` in `GET /api/v1/auth/tokens` | Field removed. Use `token_prefix` as the identity |

Existing tokens keep working: the plaintext, the hashing scheme, and the `Authorization: Bearer` shape are all unchanged. Only their storage and their delete path moved.

### Removed environment variables

These were consumed only by the legacy edge-push collector, whose code was already unreachable. Setting them had no effect before this release either.

`SERVER_URL`, `RETRY_ATTEMPTS`, `RETRY_DELAY_SECS`, `HEALTH_CHECK_INTERVAL_SECS`

### New environment variables

Set by the chart. Listed for anyone running the binary directly.

| Variable | Mode | Notes |
| --- | --- | --- |
| `SCRAPER_URL` | server | Required. The server holds no database |
| `INTERNAL_TOKEN` | both | Must match on both pods. Empty makes the scraper reject every request |
| `INTERNAL_PORT` | scraper | Default `8081` |
| `NOTES_CONFIGMAP` | server | Default `trivy-collector-notes` |
| `API_TOKENS_SECRET` | server | Default `trivy-collector-api-tokens` |

`STORAGE_PATH` still exists but is now scraper-only.

### `/swagger-ui` is gone

The API reference is now an embedded [Scalar](https://scalar.com/) build. Update bookmarks and any links that pointed at the old path.

| Before | After |
| --- | --- |
| `/swagger-ui` | `/api-docs` |
| `/api-docs/openapi.json` | unchanged |

The Scalar bundle ships inside the image rather than loading from a CDN, so the page works with no egress. Two upstream defaults are overridden deliberately, both verified by watching what the page actually requests rather than by reading the configuration:

- `telemetry` defaults to true, and does **not** gate Scalar's registry lookups. Those come from `externalUrls.apiBaseUrl`, which defaults to `api.scalar.com`, and fired on every page load until pinned to this origin.
- `showDeveloperTools` defaults to showing Share and Deploy actions on localhost. Those lead into Scalar's hosted platform.

The page also carries a `Content-Security-Policy` with `connect-src 'self'`. Configuration asks a dependency not to call out; the header removes its ability to, which is what survives a future version adding an endpoint the configuration says nothing about. If you front this app with a proxy that rewrites CSP, leave that header alone.

### The header became a sidebar

Navigation moved from a horizontal header to a foldable sidebar, and the fold is remembered per browser in `localStorage`. Two sets of pages that were previously reachable only from inside another page are now top-level entries: the search pages (CVE, Component) and the admin pages (Clusters, Alerts). The admin tab strip is gone.

`main` no longer caps content at 1400px, so tables use the full window width. Folding the sidebar gives that width back to the content, which is the point of the control.

### A misconfigured scraper stops claiming it is rebuilding

A scraper with `scraper.watchLocal: false` and no registered edge clusters previously reported `hydrated: false` forever, so the dashboard showed a "Rebuilding report data" banner that could never clear. Hydration now distinguishes "nothing to watch" from "still syncing", and the UI says which one it is. `GET /api/v1/hydration` gained a `watching` field; a missing field is read as `true`, so a new server works against a pre-1.7.0 scraper during a rolling upgrade.

### Behavior changes to expect

- **The dashboard shows a rebuilding banner after a scraper restart.** The report set is genuinely incomplete until every registered cluster replays its initial list. The scraper's `/readyz` fails for that window, and MCP query tools return a retryable error rather than a confidently empty result.
- **`trivy_collector_db_size_bytes` sawtooths across restarts.** The database starts empty every time by design. Watch it against `scraper.storage.sizeLimit`, not as a growth trend.
- **The server reports not-ready when the scraper is unreachable.** A wrong `SCRAPER_URL` or a mismatched `INTERNAL_TOKEN` now fails the rollout instead of rolling out green and returning 502s.
- **Alerts fire again.** Evaluation moved onto the scraper's ingest path. In the previous topology the evaluator sat behind an HTTP route the scraper never called, so alerts did not fire at all. Expect real deliveries once hydration completes. Review your rules and their cooldowns before upgrading if that is unwelcome.
- **Alert evaluation is suppressed until hydration completes**, so a rebuild does not re-fire every finding in the fleet as net-new.

### Removed metrics

| Removed | Replacement |
| --- | --- |
| `trivy_collector_api_logs` | None |
| `trivy_collector_api_logs_cleanup_runs_total` | None |
| `trivy_collector_api_logs_cleanup_deleted_total` | None |
| `trivy_collector_reports_sent_total` | None. The edge-push path is gone |
| `trivy_collector_reports_send_duration_seconds` | None |
| `trivy_collector_send_retries_total` | None |
| `trivy_collector_watcher_events_total` | None |
| `trivy_collector_server_up` | None |

Database gauges moved from the server to the scraper, and `trivy_collector_db_reports_total` is now `trivy_collector_db_reports`. New: `trivy_collector_notes_configmap_bytes`, `trivy_collector_api_tokens`, `trivy_collector_clusters`, `trivy_collector_fleet_hydrated`. See [Metrics](metrics.md).

Update dashboards and alert rules that reference the removed names.
