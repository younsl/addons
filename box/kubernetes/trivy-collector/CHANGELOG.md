# Changelog

Notable changes to trivy-collector, newest first.

The app and the chart version independently, and each releases when its own version value changes on merge to `main`. Entries are headed by both, since an operator upgrades them together.

Migration steps live in [docs/upgrading.md](docs/upgrading.md), not here. This file says what changed. That one says what you have to do about it.

## app 1.8.0 / chart 0.13.0

Alert rules become Kubernetes objects.

### Added

- `AlertRule` custom resource in the `trivy-collector.security.io` API group (`v1alpha1`, namespaced, plural `alertrules`, short name `tcalert`). One object per rule, so `kubectl get alertrules` shows exactly what the UI writes and a rule can be applied from Git.
- A `status` subresource carrying what the collector observed: `lastFiredAt`, `lastFiredWorkload`, `lastFindingCount`, `matchingWorkloads`, `firedCount`, `observedGeneration`, and `Ready` / `Delivered` conditions. `kubectl get alertrules` prints `READY`, `FIRED`, and `LAST-FIRED`.
- `Ready=False` with reason `InvalidVersionExpr` on a rule the evaluator cannot parse, or `Disabled` on one that is switched off. Such a rule used to be silently inert: listed, apparently enabled, never firing.
- `Ready` is answered by a watch on the rules, not as a side effect of firing, so a correct rule watching a package nobody runs reports `Ready=True` with `firedCount: 0` rather than a blank column, and a new or edited rule is answered within a watch event. Readiness deliberately does not ride the report ingest path: evaluation is suppressed until hydration completes and a report only arrives when Trivy Operator rescans, so a freshly created rule would otherwise stay blank for hours. A newer `metadata.generation` is re-acknowledged even when the verdict is unchanged.
- One-shot import of the `trivy-collector-alerts` ConfigMap on startup. Idempotent, skips names that already exist, stamps the ConfigMap with `trivy-collector.security.io/migrated-at`, and never deletes it.
- `trivy-collector crd` prints the CustomResourceDefinition as JSON for `kubectl apply -f -`. `make crd` renders the same definition into the chart, so the schema is authored once, in Rust.
- Chart values `crds.install`, `crds.keep`, `crds.annotations`, and `crds.additionalLabels`. The CRD ships in `templates/`, not `crds/`, because Helm never upgrades anything in `crds/`.
- MCP tools `list_alert_rules` and `get_alert_rule`, both gated on `alerts:get`. Slack webhook URLs are replaced with `[redacted]` rather than dropped. `list_alert_rules` takes `not_ready_only` to find rules that look active but never fire.
- [docs/alerts.md](docs/alerts.md), with an architecture diagram.

### Changed

- Alert writes are server-side applies with the field manager `trivy-collector`, so create and edit are one call and the UI can edit a rule a GitOps controller still claims fields on.
- The API server validates every rule against the CRD schema before it is stored. Field names, types, and requiredness are no longer checked only in application code.
- Audit fields (`createdBy`, `updatedAt`, `updatedBy`) live on the status subresource. An annotation is part of the spec object, so recording an edit there would read as drift to whatever GitOps controller owns the manifest. `created_at` comes from `metadata.creationTimestamp`, which the API server owns, so it cannot be backdated by replaying an old payload.
- `GET /api/v1/alerts` replaced the `configmap` field with `api_version` and `resource`. `items` is unchanged: the HTTP API stays snake_case while the stored object is camelCase, converted in one place.
- Alert responses carry real OpenAPI schemas (`AlertListResponse`, `AlertTestResponse`, `AlertRule`) instead of an untyped `200`.
- The chart `Role` gained `trivy-collector.security.io` `alertrules` and, as a separate rule, `alertrules/status`.

### Fixed

- The OpenAPI document reports the status codes the alerts, hub, notes, and token endpoints actually return. Previously undocumented `400`, `422`, `500`, `502`, and `503` cases now appear, a `400` the SBOM component suggest endpoint never returns is gone, and `POST /api/v1/auth/tokens` declares the request body it takes.
- The OIDC `/auth/login`, `/auth/callback`, and `/auth/error` routes are documented rather than served but absent from the spec.
- A Slack delivery failure is recorded on the rule as `Delivered=False` with the receiver and error, instead of only a log line.

## app 1.7.0 / chart 0.12.0

The PersistentVolume is gone and the UI is reorganised. See [docs/upgrading.md](docs/upgrading.md) for the export you must run first: API tokens and report notes are unrecoverable.

### Added

- `migration.exportState` pre-upgrade hook Job, which reads the legacy database and writes API tokens to a Secret and report notes to a ConfigMap.
- Per-cluster hydration tracking, surfaced on `GET /api/v1/hydration`, on the scraper's `/readyz`, and as a rebuilding banner in the UI.
- Embedded [Scalar](https://scalar.com/) API reference at `/api-docs`, with the bundle shipped in the image and a CSP that removes its ability to reach Scalar's hosted platform.

### Changed

- The scraper owns SQLite on its own `emptyDir` and serves it to the server pods over an internal API on `:8081`. The server holds no database and no volume, so it scales past one replica.
- Reports are treated as a mirror: a scraper starting empty relists them from the clusters that own the CRs.
- Alert evaluation moved onto the scraper's ingest path. In the previous topology the evaluator sat behind an HTTP route the scraper never called, so alerts did not fire at all. Evaluation stays suppressed until hydration completes.
- Navigation folded from a horizontal header into a sidebar. The search and admin pages became top-level entries.
- Database size comes from the SQLite page pragmas rather than `fs::metadata`, so it reports the committed logical size and not an uncheckpointed WAL segment.

### Removed

- `server.persistence.*`. The release provisions no PVC.
- `server.ingress.*`, replaced by `server.gateway.*` (Gateway API `HTTPRoute`).
- `/swagger-ui`. The OpenAPI document stays at `/api-docs/openapi.json`.
- The API-log table and its admin listing. Requests are structured stdout lines, queried through the cluster's log pipeline.

## app 1.6.0 / chart 0.10.0

### Added

- Embedded MCP server on an opt-in `/mcp` endpoint (Streamable HTTP), so LLM agents such as [kagent](https://kagent.dev/) can query reports under the same auth and RBAC as the API. See [docs/mcp.md](docs/mcp.md).

## Earlier

Releases before 1.6.0 predate this file. The chart's `artifacthub.io/changes` annotation in `charts/trivy-collector/Chart.yaml` carries the per-release notes that were published at the time.
