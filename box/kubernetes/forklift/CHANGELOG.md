# Changelog

Notable changes per release. Container image versions come from the
`org.opencontainers.image.version` label in the `Dockerfile`; chart versions from
`charts/forklift/Chart.yaml`.

## Unreleased

### Added

- `cargo publish`, `cargo yank` and `cargo yank --undo` on hosted Cargo repositories. The sparse `config.json` of a hosted repository now advertises `api`, and the Registry Web API publish body is adapted onto the same validated, atomic publication service as the UI and upload API, like `npm publish` and Twine. Identity, dependencies and features still come from the `.crate`'s normalized `Cargo.toml`. Yank requires `write`, matching the UI action. Owners and every other unimplemented Web API route answer `404` in cargo's error envelope. Proxy and group repositories stay read-only.
- `cargo search` on hosted Cargo repositories (`GET api/v1/crates?q=&per_page=`). It matches crate names, ignoring case and hyphen/underscore differences, ranks exact then prefix matches first, and leaves fully yanked crates out. Publishing now records the `[package].description` in the crate's metadata so search can show it. A group's `config.json` advertises the group's own `api`, so `cargo search` works through `cargo-public` (answered by its first hosted member) while a publish through it is refused as read-only. Proxies answer `404` and advertise no `api`.
- The Cargo sparse registry and Registry Web API endpoints are in the OpenAPI document under the `cargo` tag, with a `cargoToken` security scheme for the bare token. They carry `x-codegen-skip`, which the console's client generator now honours.
- A bare personal access token in `Authorization` (no scheme) now authenticates. Cargo's `cargo:token` provider sends the token verbatim on publish and on every `auth-required` index read, so `CARGO_REGISTRIES_<NAME>_TOKEN=flpat_...` works without a `Bearer ` prefix. Any other scheme-less value stays anonymous.

- Labeling coverage on a repository's Statistics tab: how many artifacts carry at least one label, as a count and a percentage of all artifacts. It is counted over the whole repository, not the 500-artifact sample the scan panels use, from the new `labeled_count` field of the artifact listing (`GET /api/v1/repositories/{id}/artifacts`), which ignores the active search.
- Hovering the Yank and Unyank buttons of a Cargo version explains what they do, since neither removes the crate.
- Every Statistics panel has a magnifier that opens the Artifacts tab filtered to what the panel counts, with the filter in the URL (`?filter=`) and a removable filter chip. The artifact listing takes the matching `filter` parameter (`labeled`, `scanned`, `clean`, `vulnerable`, `licensed`, `broken`). `labeled` covers the whole repository. The others cover the 500 most recently accessed artifacts, the same sample their panels aggregate, so the drill-down always lists the number the panel shows. `filtered` counts the matches and `limit`/`offset` page within them.

### Changed

- The web console takes Apple's visual language ([docs/designs/apple-inspired.md](docs/designs/apple-inspired.md)): Apple's system neutrals in both themes (pure black and `#1c1c1e`-`#3a3a3c` in dark, white cards on `#f5f5f7` parchment in light), the system font stack (SF Pro on Apple devices, Noto Sans KR elsewhere) with Apple's tracking, pill-shaped primary buttons, badges and search fields, 8px form controls, 18px cards, a press-to-scale button state and no chrome shadows. The accent stays forklift yellow and dark stays the default. Status text that used Tailwind's `emerald`/`amber` palette now uses the status tokens, fixing three light-mode contrast failures. No text falls below WCAG AA on the audited routes in either theme.
- Repository `publish_methods` includes `cargo` for Cargo repositories.

### Fixed

- A group's Cargo `config.json` pointed `dl` at its first member, so every `.crate` download bypassed the group and reached only that member. Crates held by later members, such as the `crates-io` proxy behind `cargo-public`, answered `404`. `dl` now names the group.
- `GET api/v1/crates` on a group, which has no trailing slash, was classified as a sparse-index entry and aggregated. It now fans out like any other Web API call.

## Chart 0.13.1 (2026-09-18)

forklift remains 0.13.3. forklift-mcp remains 0.3.2.

### Changed

- Every probe spells out `timeoutSeconds` and `failureThreshold` rather than
  inheriting the Kubernetes defaults. The rendered pod is unchanged; the point
  is that these are load-bearing values, not incidental ones. The readiness
  probe's 1s timeout in particular is paired with the handler's own 750ms bound
  on the database check, which answers "not ready" quickly instead of letting
  the probe give up with no reason recorded.

## Chart 0.13.0 (2026-09-18)

forklift remains 0.13.3. forklift-mcp remains 0.3.2.

### Added

- Startup probe on the forklift container, enabled by default with a 300s
  budget. The S3 metadata snapshot is restored and schema migrations are
  applied before the HTTP listener binds, so boot time grows with the snapshot
  and with any index-building migration. Until now the liveness probe started
  counting immediately and killed a boot that took longer than 25s; because
  `/data` is ephemeral and the leader keeps publishing the pre-migration
  snapshot, every restart re-ran the migration from scratch and the pod never
  recovered on its own. 0.13.2's `0039_artifact_download_counts` index hit this
  on a 1.7 GiB snapshot with 6.5M audit rows.

### Changed

- Breaking: `livenessProbe` and `readinessProbe` are now nested under `probes`,
  keeping their names, alongside the new `probes.startupProbe`. Values files
  that set either key at the top level must be updated; the top-level keys are
  no longer read.

## 0.13.3 (2026-09-16)

Chart 0.12.4. forklift-mcp 0.3.2.

### Changed

- Every duration histogram gained a 2s bucket, which the default set skips
  between 1s and 2.5s: HTTP requests, the readiness probe, upstream fetches,
  metadata rewrite wait and hold, blob store operations, and MCP tool calls.
  All existing bucket edges are kept, so recorded quantiles and alert
  expressions stay valid. The added edge is one extra series per existing
  label combination, so the repository, backend, and tool label sets multiply
  it.

## 0.13.2 (2026-09-11)

Chart 0.12.3. forklift-mcp remains 0.3.1.

### Added

- Artifacts table columns for downloads in the last 30 days and the last
  recorded authenticated user, with sorting and username search.
- Indexed download aggregation from retained audit logs, counting GET 200/206
  responses for each repository and path. Group requests are not attributed to
  members; counts depend on available audit history.

### Fixed

- README license badge uses an explicit Apache-2.0 label.

### Changed

- API documentation describes download counting, anonymous access handling,
  searchable fields, and sorting within the currently loaded page.

## 0.13.1 (2026-09-09)

Chart 0.12.2. forklift-mcp 0.3.1.

### Fixed

- `npm publish` of a scoped package (`@scope/name`) to a hosted repository
  returned `400 Bad Request` since 0.13.0. npm clients name the publish
  attachment `<name>-<version>.tgz`, so a scoped package arrives as
  `@scope/name-1.0.0.tgz`; the native upload path forwarded that string as the
  multipart filename and the receiver rejected the path separator. The Go
  implementation stripped the directory implicitly through
  `multipart.Part.FileName`, which the Rust port did not reproduce. The
  attachment name is now reduced to its basename before it enters the upload
  contract, and a regression test publishes a scoped package end to end.
- forklift-mcp rejected tool calls from clients that serialise every argument
  as a string. Google ADK's `adk-mcp-client`, and kagent on top of it, send
  `"7"` where the input schema says integer and `"true"` where it says
  boolean, so deserialisation failed with `invalid type: string "7", expected
  i64`. Arguments are now coerced towards the schema's declared type
  (integer, number, boolean, array, object, and array items) before the tool
  runs; values that already match, or cannot be converted, are left as they
  are so the usual validation error still surfaces.

### Changed

- Web development dependencies: vitest 4.1.11, plus override pins for js-yaml
  (3.15.2, 4.3.2) and hono (4.13.5), clearing the advisories that stopped the
  `pnpm audit` gate in the release workflow. None of these ship in the image.

## 0.13.0 (2026-09-08)

Chart 0.12.0. forklift-mcp 0.3.0.

### Changed

- Forklift and Forklift MCP use Rust 1.98.1. HTTP routes, SQLite migrations,
  authentication cookies, access tokens, and repository formats remain
  compatible with existing deployments. Build metrics report `rust_version`
  and standard `process_*` collectors.
- The profiling listener exposes CPU profiles in pprof protobuf format,
  plus the endpoint index and command line. Use `process_resident_memory_bytes`
  and `container_memory_rss` for memory diagnostics.
- The container images are built by cross-compiling to
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with
  [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) on the build
  platform, so a multi-arch build still needs no QEMU. The runtime stage is
  unchanged: a `scratch` image holding one static binary and the CA bundle.
- Browser tests launch the Rust binary with isolated metrics and profiling
  settings. OpenAPI client generation points to `src/openapi/openapi.yaml`,
  and the container builders install the pinned Rust 1.98.1 compiler with
  rustup while official container tags catch up with the patch release.
- OCI image rows support multi-selection for adding and removing labels,
  with permission checks and failed selections retained for retry.

### Fixed

- Return SQLite read-pool connections when async requests are cancelled or
  blocking operations panic.
- Update fast-uri, qs, and browserslist to resolve eight Dependabot alerts
  in web development dependencies. CI and the Forklift image release check
  development dependencies with pnpm audit.

### Security

- The OIDC dependency graph retains the unpatched RSA timing advisory
  RUSTSEC-2023-0071. Forklift verifies ID tokens using public keys and does not
  perform RSA private-key operations. See `docs/testing/release-validation.md`.

## 0.12.2 (2026-09-03)

Chart 0.11.2. forklift-mcp 0.2.1.

### Changed

- S3-mode metadata sync no longer snapshots and uploads an unchanged database.
  The leader pins one read-pool connection and compares SQLite's `data_version`
  against the value recorded at its last successful upload; when nothing was
  committed the cycle is counted as `forklift_objstore_meta_uploads_total{result="unchanged"}`
  and neither the `VACUUM INTO` nor the PutObject runs. Standbys HEAD the
  snapshot object first and re-download only when its ETag changed
  (`forklift_objstore_meta_downloads_total{result="unchanged"}` otherwise). On a
  1 GiB metadata database at the default 30s cadence, the previous behaviour
  rewrote and re-read the whole file every cycle on both pods, which showed up
  as a 2 GiB page-cache sawtooth in `container_memory_working_set_bytes` and
  roughly 70 MiB/s of sustained pod egress.
- Audit events are written in batches of up to 256 per transaction instead of
  one transaction per event, so a request burst takes the single SQLite write
  connection once per batch. This is what stopped the recorder's buffer from
  overflowing (`forklift_audit_events_dropped_total`) while artifact writes held
  the connection.
- Rendered npm packuments stay cached for up to 15 minutes instead of 30
  seconds. The 30-second bound existed only so a version leaving the age-policy
  cooldown would surface promptly; the rewrite now reports the exact instant the
  next blocked version is released and the cache entry expires then, so the
  short bound was replaced by a precise one. Every other render input (stored
  bytes, host, serving repository, configuration revision) was already part of
  the cache key. Measured hit ratio before the change was 36%, and every miss is
  a decode holding a rewrite slot.

### Added

- The HA Status page reports the Rust toolchain the running binary was built
  with, next to the forklift version. `GET /api/v1/ha` carries it as `runtime`.
- A CPU profiling listener on `FORKLIFT_PPROF_ADDR` (`127.0.0.1:6060` by
  default, empty disables). It is loopback-only on purpose: the metrics port is
  published through the Service, and profiling endpoints are unauthenticated.
  Reach it with `kubectl port-forward`; see the Profiling section in
  `docs/metrics.md`.

## 0.12.1 (2026-08-30)

Chart 0.11.1. forklift-mcp 0.2.1.

### Changed


- The console's design tokens were rebuilt on a contrast-anchored ramp ported
  from the v2 Figma foundation. A step number is now a promise about contrast
  against that theme's canvas, and the same step means the same promise in both
  themes, so one semantic name serves dark and light. Audited against the DOM of
  all 24 routes: v1 had 147 real contrast violations, 108 of them caused by the
  token values themselves, and v2 leaves none of those. The accent is split into
  fill, foreground and ink roles so the brand yellow survives light mode, and
  `styles.css` was split so the file holding design tokens holds nothing else.
  Dark mode keeps its black canvas: the ported ramp ran warm, so the dark
  neutrals were re-derived on a neutral hue from `#0a0a0b`, holding the ramp's
  step ratios (1.04 / 1.11 / 1.22 / 1.49 for neutral 100-400) rather than v1's
  collapsed stack where canvas and sidebar measured 1.012.

### Fixed

- Patched vulnerable backend and web dependencies in the container images.

## 0.12.0 (2026-08-25)

Chart 0.10.0. forklift-mcp 0.2.0.

### Added

- Bulk actions on the Artifacts tab. A repository holds tens of thousands of
  artifacts and every operation on them was one row at a time, so labelling a
  release or clearing a batch of caches was work nobody did. Rows are now
  selectable across pages and sorting, and one **Actions** menu applies to the
  whole selection: add a label, remove a label, delete.

  Two endpoints back it, `POST /repositories/{id}/artifacts/labels/bulk` and
  `POST /repositories/{id}/artifacts/bulk-delete`, each capped at 200 paths per
  request and each reporting per path rather than as one verdict: a selection of
  hundreds routinely holds a few paths the caller may not touch or that another
  request has already removed, and failing the whole batch for those leaves
  nobody able to make progress. Permission is the single-artifact one applied per
  path, so labelling spans only the artifacts the caller owns, and every attempt
  is audited exactly as a single one is. The console sends a larger selection in
  batches, folds the answers into one report, and leaves the refused paths
  selected so the next click is a retry of those.

  Deleting a selection asks for the word "delete" to be typed first. There is no
  force in the batch: an artifact whose bytes are gone is repaired from its own
  row, which is the one delete still there.

  Both operations are MCP tools as well, `forklift_bulk_label_artifacts` and
  `forklift_bulk_delete_artifacts`, bringing that server to 70 tools.

- forklift-mcp now covers every read operation of the management API, 68 tools
  in total (was 37). The whole coverage surface is exposed: dashboard, groups,
  history, project detail, last commit and pipeline, mute, settings get/update,
  host and GitLab checks, scan start, and report preview/send. The remaining
  read-only views followed: upstream health, OCI tags and artifact detail,
  artifact labels, dangling artifacts, repository permissions and tokens,
  approval count and pending repositories, own tokens, notification receivers,
  announcement, landing stats, repository names, repository alarm preview,
  upload session state and `forklift_whoami`. Every tool is pinned to its API
  operation by test.

- Coverage exclusions are now per check. CI and registry are muted
  independently on the project page, and a muted check is neither required for
  the verdict nor credited to it, so a project with no packages to pin counts as
  applied on its CI alone instead of sitting at partial forever or being dropped
  from the measurement entirely. Muting both is the whole project out, which is
  what a bare mute always meant, and what every existing mute migrates to. The
  mute API takes a `scopes` list alongside the old boolean, and
  `forklift_set_coverage_project_muted` takes the same. The GitLab exclude topic
  stays all-or-nothing.

### Changed

- Table pagination gained first and last page buttons and says which page it is
  on, so a table of three hundred pages can be crossed in one click instead of
  three hundred. The four moves are icons of one shape, with the words in their
  labels. Shared by every paged table, so the approvals list gained the same.
- A manual **Scan now** no longer posts the coverage report. Only the automatic
  scans do, the scheduled one and the first scan after a cold start; sending on
  demand is **Send now** on the coverage settings page, which posts what the
  last completed scan measured. Asking the console for the current number is not
  asking to tell the receiver about it.
- Chart `mcp.image.tag` defaults to `0.2.0`.

## 0.10.0 (2026-08-22)

Chart 0.7.0.

Coverage scanning: forklift now measures the half of its own rollout it could
not see. An instance knows the projects that already pull through it and nothing
at all of the ones still resolving straight from the public registries, so the
number an operator actually wants, how much of the organisation builds through
forklift, could only be answered from the source side. It walks a GitLab
instance, reads each project's CI and package-manager configuration, and reports
what is wired, what is half wired, and what is still to migrate.

### Added

- Coverage scanning, under Workspace > Coverage. A project counts as applied
  when both halves of the wiring are present: its CI pipeline references
  forklift (by host, or through the `FORKLIFT_*TOKEN` a job authenticates with)
  and a package-manager or image-build file pins the registry. One of the two
  makes it partial, which is the interesting state, since those builds resolve
  through forklift on some paths and around it on others. A project with no
  GitLab CI at all was never a candidate and is counted apart, outside the
  denominator. Evidence is kept per project: the files that matched, the branch
  the verdict came from, and the repository formats read out of the forklift
  URLs.
- Two views of the same scan, each with its own address: the trend and project
  list at `/workspace/coverage` (filterable by verdict, and the filter is in the
  URL so it can be linked to), and the per-group breakdown at
  `/workspace/coverage/groups`. Every signed-in user can read them; running a
  scan and changing what is measured are administrator-only.
- A scheduled report naming the projects still to migrate, delivered through the
  existing notification receivers rather than a second webhook configuration.
  It can be previewed and sent by hand, and skipped automatically at full
  coverage, since a report nobody has to act on is noise.
- Adaptive request concurrency for the crawl, using the AIMD loop Vector uses:
  the in-flight limit rises while GitLab keeps answering promptly and is cut
  multiplicatively on a 429, a 5xx, or a round-trip time that has climbed clear
  of its own recent average. There is deliberately no rate setting, because the
  safe rate depends on the instance and the hour; the concurrency the scan
  settled on is logged when it completes.
- `FORKLIFT_COVERAGE_ENABLED`, `FORKLIFT_COVERAGE_GITLAB_URL` and
  `FORKLIFT_COVERAGE_GITLAB_TOKEN`, mapped from `coverageScanning.gitlab.*` in
  the chart. The switch is separate from the credentials on purpose: a GitLab
  token already in a deployment's environment must not start a crawl because
  forklift gained the ability to. The token stays in the environment and never
  reaches the metadata database, its object-storage snapshots, or an API
  response.
- Migration 0036 (`coverage_settings`, `coverage_results`, `coverage_history`,
  `coverage_muted`). It applies on startup and is backward compatible: an older
  binary ignores the tables.

### Changed

- The forklift host a project must reference is derived from
  `FORKLIFT_EXTERNAL_URL` rather than configured, and is checked as it is typed
  when overridden: the shape is validated locally and blocks saving, while DNS
  is resolved by the server and only warns, since forklift resolves names from
  inside the cluster and the builds it measures resolve them from wherever they
  run.

### Fixed

- The GitLab client no longer follows redirects. The HTTP client does not treat
  `PRIVATE-TOKEN` as a sensitive header, so a redirect would have handed the
  access token to whatever host the instance named.

## 0.9.0 (2026-08-20)

Chart 0.6.0.

Per-artifact labels, and the metadata store stops being a single queue. An
instance was reported unreachable and recovered only by restarting the pod: the
console, package traffic and the readiness probe all shared one SQLite write
connection, so ordinary write contention failed `/readyz` and took the pod out
of the Service while the process itself was healthy. Reads now use the read
pool, the two whole-table aggregates behind the repository list are indexed and
cached, and the saturation that caused it is now measurable.

### Added

- Artifact labels: short operator tags on one stored artifact, the same for
  every format because the identity is the repository plus the stored path.
  Adding or removing one is allowed for an administrator on the repository and
  for the principal recorded as having uploaded that artifact, and every
  attempt is written to the repository's audit log as `artifact.label.add` or
  `artifact.label.remove` with the label in the entry detail, refused attempts
  included. Backed by `GET|POST|DELETE /repositories/{id}/artifacts/labels`;
  the artifacts API and the OCI views carry each artifact's labels with the
  caller's per-artifact permission. Labels are searchable from the sidebar as
  their own result group and match when filtering a repository's artifact
  table; a label is removed with the artifact it describes. A label is a bare
  key or a `key:value` pair, each side letters, digits, `-` and `_`, at most 64
  characters, 20 per artifact.
- Metadata connection-pool metrics (`forklift_db_connections_*`,
  `forklift_db_connection_waits_total`,
  `forklift_db_connection_wait_seconds_total`), readiness observability
  (`forklift_readyz_failures_total` by reason,
  `forklift_readyz_duration_seconds`) and repository-list aggregate cost
  (`forklift_scan_ratio_lookups_total`, `forklift_scan_ratio_refresh_seconds`).
  Because every write shares one SQLite connection, waiting for it is the
  saturation signal an overloaded instance shows, and it was previously
  invisible.
- Copy icons on package identities in the Artifacts tab (artifact path,
  publication coordinate, OCI image name, tag reference and digest), appearing
  on hover to the right of the value they copy.

### Changed

- The repository list stopped recomputing its scan-coverage aggregate on every
  request: the pass reads every versioned artifact plus the whole scan table, so
  a few open consoles were enough to keep the database busy. It is now cached for
  30 seconds, and two covering indexes
  (`artifacts(repo_id, size)`, `artifacts(repo_id, version, path)`) let the list
  view's aggregates be answered from an index instead of a table scan.

- Schema migrations 0034 (`artifact_labels`, with a foreign key onto
  `artifacts(repo_id, path)` so labels are removed with the artifact they
  describe) and 0035 (the two covering indexes above; `idx_artifacts_repo` is
  dropped, being a prefix of the new one). Both apply on startup and are
  backward compatible: an older binary ignores the table and the indexes.

### Fixed

- Read-only queries no longer run on the single write connection. 58 statements
  (the console's repository list and artifact search, audit log reads, user and
  role lookups, the scrape-time aggregates) queued behind whichever write held
  the one writer, and the readiness probe pinged that same connection: ordinary
  write contention could therefore fail `/readyz`, take the pod out of the
  Service and make the whole instance look down while it was only busy. Reads
  now use the read pool, `/readyz` pings it with its own 750ms bound, and a
  failed probe is logged with a reason.

## 0.8.0 (2026-08-20)

Chart 0.6.0.

Native OCI registry support: one binary now serves container images and Helm
charts alongside the five package formats, verified against the official OCI
distribution-spec conformance suite v1.1.1 (74 passed, 0 failed across the
pull, push, content discovery and content management workflows) and exercised
end to end with crane, helm and podman.

### Added

- OCI repository format (`oci`) with hosted, proxy and group types, served
  under `/v2/` with the repository name as the first image-name segment
  (`docker pull host/oci-public/library/nginx:1.27`). Push supports monolithic
  and chunked blob uploads, cross-repository mounts and multi-platform
  indexes; upload sessions persist in the metadata store so a push survives
  being load-balanced across replicas. The OCI 1.1 referrers API (`end-12`)
  is included, with `OCI-Subject` acknowledged on push.
- OCI proxy repositories perform the upstream bearer-token handshake (Docker
  Hub, GHCR, Quay) with a per-scope token cache, and apply Docker Hub's
  `library/` rewrite for single-segment names. Upstream 401/403 responses are
  relayed as 404 so group lookups fall through, matching how public registries
  answer for absent images.
- OCI garbage collection by reachability: a leader-gated prune removes
  untagged manifests and unreferenced blobs (referrers attached to live
  manifests survive), replacing the idle reaper and LRU eviction, which are
  disabled for the format because either would break still-tagged images. New
  metrics `forklift_oci_prune_deleted_total` and
  `forklift_oci_upload_sessions_active`; OCI proxies report through the shared
  cache hit/miss, byte and upstream-latency metrics like every other format.
  New settings
  `FORKLIFT_OCI_MAX_MANIFEST_BYTES`, `FORKLIFT_OCI_MAX_BLOB_BYTES`,
  `FORKLIFT_OCI_UPLOAD_SESSION_TTL` and `FORKLIFT_OCI_PRUNE_INTERVAL`.
- Harbor-style OCI console: the Artifacts tab lists tagged images (kind,
  platforms, size, digest, push/pull time and actor) and each artifact opens a
  dedicated page with an Overview section and kind-specific additions - chart
  README rendered GitHub-style with syntax-highlighted values.yaml and
  Chart.yaml, image config summary, index children. Backed by
  `GET /repositories/{id}/oci-tags` and `GET /repositories/{id}/oci-detail`.
- Per-repository visibility: a public repository serves anonymous downloads
  while writes keep requiring authentication, controlled from the creation
  form and the Settings tab (`config.public`), alongside the instance-wide
  `FORKLIFT_ANONYMOUS_READ`. New repositories stay private by default.
- Optional repository descriptions, bilingual for the seeded set; the console
  shows the viewer's language.
- Seeded OCI trio: `docker.io-proxy` and `ghcr.io-proxy` proxies plus
  `oci-hosted`, combined behind the `oci-public` group. Installs seeded with
  the earlier `docker-hub`/`ghcr` names are renamed in place.
- Artifact provenance: `last_accessed_by` records who last downloaded an
  artifact (throttled with `last_accessed_at`), surfaced in the OCI views and
  the artifacts API.
- Users console: the Quotas panel shows the consecutive failed-login count
  against its hard limit (marked not applicable for robot accounts), and the
  user API exposes `failed_login_count`.
- Login page welcome line backed by the public `GET /api/v1/stats/landing`
  totals; announcement banners gained a "hide for 7 days" snooze that a
  changed notice overrides.
- CI runs the OCI conformance suite on every pull request.

### Changed

- Security tab shows only the policies that operate on the repository's
  format: the vulnerability and license gates are hidden for OCI (OSV and
  deps.dev have no container-image ecosystem) as the age policy already was
  for hosted repositories. Hidden steps persist through configuration
  round-trips.
- Danger-zone sections across the console carry a red border; assorted
  spacing, sidebar and repository-list layout tightening (visibility column,
  copy-only endpoint column).

## 0.7.2 (2026-08-18)

Chart 0.4.2.

Metadata serving under an install burst. An installer resolving a few hundred
packages could see every request to a proxy repository fail with no status code
at all, only its own fetch timeout, while forklift's upstream and object store
latencies stayed in the low milliseconds and its CPU stayed idle.

### Fixed

- npm packument responses no longer hold a rewrite slot while they are written to
  the client. The gate has four slots, so clients that read slowly held all of
  them and every other metadata request queued behind them, including cache hits.
  Both npm and pnpm arm their fetch timeout when a request is created rather than
  when it reaches a connection, so a queue this deep expires the whole backlog at
  once, tarball requests included: they wait in the installer's own connection
  pool behind the stalled packuments.

### Added

- Rendered packuments are reused for 30 seconds, keyed by the stored bytes, the
  external base URL, the serving repository and the repository configuration. A
  repeat request (a second job resolving the same lockfile, or a retry after a
  timeout) skips both the decode and the gate. The cache is in-process and capped
  at 64 MiB.
- Metrics for the metadata rewrite gate, which had no observability at all:
  `forklift_metadata_rewrite_wait_seconds`,
  `forklift_metadata_rewrite_duration_seconds`,
  `forklift_metadata_rewrite_abandoned_total`,
  `forklift_metadata_rewrite_inflight`, `forklift_metadata_rewrite_queued`,
  `forklift_metadata_rewrite_capacity` and
  `forklift_metadata_render_cache_total`. The abandoned counter is the one that
  records requests whose client gave up while queued, which appear nowhere in
  `forklift_http_requests_total` because they never get a response.
- `BenchmarkPackumentRewrite` guards the per-request cost of serving a packument.

### Changed

- Rewriting a packument splices the `dist.tarball` string in place instead of
  decoding each version manifest into a map and re-encoding it. Measured 1.5x
  faster on a 284 KB, 500 version document (9.44 ms to 6.30 ms). Every other byte
  of a version manifest now reaches the client exactly as upstream published it,
  where the previous path reordered its fields alphabetically.
- A tarball fetch no longer re-reads and re-scans the whole packument to recover
  the version's publish time when the repository's age policy is disabled. With
  the policy off that timestamp only fills artifact metadata, for which the
  upstream `Last-Modified` header is already the documented fallback. Repositories
  with the age policy enabled are unaffected.
