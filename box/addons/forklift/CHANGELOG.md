# Changelog

Notable changes per release. Container image versions come from the
`org.opencontainers.image.version` label in the `Dockerfile`; chart versions from
`charts/forklift/Chart.yaml`.

## 0.17.0 (2026-10-21)

### Added

- Outbound proxy and private CA support. `FORKLIFT_EXTRA_CA_FILE` names a PEM bundle of private CA certificates that every outbound client (proxy upstreams, OSV, deps.dev, OIDC, webhooks, GitLab, S3) trusts on top of the system roots. A bundle that cannot be read or holds no certificate fails startup. The chart's `outbound.caBundle` mounts it from a ConfigMap, and `outbound.proxy` sets `HTTPS_PROXY`, `HTTP_PROXY` and a `NO_PROXY` that always keeps in-cluster traffic (forklift's own Services and the bundled SeaweedFS) off the proxy. Both apply to the storage migration Job too. The startup line `outbound http configured` reports whether a proxy is set and how many extra CA certificates were loaded. See `docs/installation.md`.
- `npm audit` and `pnpm audit` against an npm repository. Forklift answers the npm bulk advisory endpoint (`POST /npm/<repo>/-/npm/v1/security/advisories/bulk`) from the OSV data the vulnerability policy uses, applying the repository's `vuln.ignore` list, so the client report and the gate agree and the dependency list never leaves forklift for the npm registry. Hosted repositories answer with no advisories, and a group leaves out the packages its hosted members publish. A failed OSV lookup fails the audit instead of reporting it clean. A request lists at most 10,000 packages, each package's advisories are cached for one hour, and at most four uncached lookups reach OSV at once. New metrics `forklift_npm_audit_requests_total` and `forklift_npm_audit_duration_seconds`. See `docs/security-policies.md`.
- Delete in the OCI Images view's Actions menu, for administrators, as other formats have in their Artifacts tab. Each selected image is deleted with every tag pointing at its manifest, behind the same type-to-confirm guard, and the reachability prune reclaims the unused layers after its 24 hour grace.

### Changed

- Rendering an npm packument takes about a tenth of the CPU it did (a 30 MB packument drops from about 850 ms to 60 ms). The JSON decoder read the cached document one byte at a time through a blocking bridge to the async reader, and the rewrite then re-parsed and re-copied every version manifest twice. The document is now read once, parsed in place and written out in a single pass, so peak memory per render is the document plus its output. The rewrite runs on the blocking pool instead of a runtime worker, and rewrite slots free up sooner, which shortens the `forklift_metadata_rewrite_wait_seconds` queue during install bursts. Streaming JSON decodes elsewhere (PyPI index, npm tarball publish time, OCI manifests) are buffered too.
- The rendered metadata cache releases expired documents on the next insert instead of keeping them resident until the byte cap forces them out.
- The `forklift` binary allocates through jemalloc on Linux instead of the musl libc allocator, which serializes threads behind one lock and is slow on the many small allocations metadata rendering makes. `forklift-mcp` keeps the libc allocator.

### Fixed

- The S3 client ignored `HTTPS_PROXY` and `NO_PROXY`, so object storage behind a forward proxy was unreachable while every other outbound request went through it.
- Deleting an OCI manifest through the artifact delete or bulk delete API left its tags behind, pointing at a manifest that no longer existed. They are now deleted with it.

### Security

- Dev-only web dependencies are pinned to patched releases: vite 8.1.6 (launch-editor advisories GHSA-587h-7wmj-h5pm and GHSA-698v-4r3x-3vcv), follow-redirects 1.16.1 (GHSA-q3jh-wrc6-jgq8, GHSA-8r9p-f939-6h3c), `@modelcontextprotocol/sdk` 1.32.0 (GHSA-22jm-h49p-29qw, GHSA-46jr-4wx8-v453 and others) and hono 4.13.11 (GHSA-5r4p-p66f-jhc7). They come in through the build toolchain, Storybook, wait-on and shadcn, and none is bundled into the web UI.

## 0.16.1 (2026-10-19)

### Added

- Insight check AC-08 Roles nobody holds (`unused-roles`, Access, Security and DevOps): a role created over 7 days ago that no user holds, no OIDC group maps to and is not the default role is Low. Roles granting admin are listed first. Instead of a repository, each row shows how many users hold the role and when it was created, with its grants and a link to the role.
- An insight check page numbers its flagged items and shows the first 100 with Show more, which loads the next 100 each time. Leaving the page collapses the list again. A finding now keeps up to 10,000 items (100 before), and `GET /api/v1/insights/checks/{id}/affected` pages through them for administrators, auditors and security reviewers.
- Recent alarms on the Security and Coverage Settings pages: the last 30 days of alarm deliveries charted with the same Metrics panels a check page uses, whether each delivery landed on the left and how many findings or projects it listed on the right, with the same crosshair tooltip (time, result, receiver, scheduled or manual, and the error of a failed one) and drag-to-zoom. A manual Send now shows up on the chart at once. Every delivery attempt, scheduled or manual, is recorded per receiver and kept for 90 days. The API is `GET /api/v1/insights/notification/history` (admin or security) and `GET /api/v1/coverage/notification/history` (admin).
- Insight check OP-04 Data volume running out of space (`storage-capacity`, Operations, DevOps): the volume holding the data directory, which keeps the metadata database and the filesystem backend's blobs, is Critical at 90% full or more, High at 80% and Medium at 70%. The flagged item names the data directory with its usage and free space and links to the Storage page. It shows as Unavailable when the volume cannot be measured. Security Insights now has 26 checks.
- The Storage page's Cluster usage bar is split into coloured parts with a legend under it: the four formats using the most space, other forklift data (smaller formats and blobs awaiting cleanup), Other data (objects other clients keep in the store, such as other buckets), System data (disk the objects do not account for: replicas or parity, unreclaimed deletes, store metadata and other files on the same disks, drawn hatched) and Free. Each legend row gives the size and the share of capacity, and hovering or focusing a part shows its share of capacity and of used space. Other data and System data sit at the left end of the bar. A store that does not report its object bytes shows the two as one Other and system data part. `GET /api/v1/storage` returns the split as `usage_by_format`.

### Changed

- The Cluster usage bar no longer turns yellow or red as a whole. The percentage in its header takes the warning or critical colour instead, and the 70% and 90% markers stay. On a phone the marker captions show only the number.
- The scheduled coverage report waits for each receiver's response, within the existing 30 second bound, so its outcome is logged and recorded.

### Fixed

- Pointing at a run in a status timeline (an insight check's Status over time, and the new Delivery result) could name a different run than the band under the pointer: a band one run wide was drawn wider than its run, pushing every later band to the right.
- A failed webhook delivery is reported with a short reason (the HTTP status, or no response from webhook) in the repository notification test, the Security and Coverage Send now results and the alarm history. The transport error it replaces could repeat the webhook URL, whose path is often the channel's secret token.

## 0.16.0 (2026-10-18)

### Added

- Security Insights. Modeled on AWS Trusted Advisor: every rule is a check listed by category with a red, yellow or green status, passing checks included, and each check has its own page with the result, why it matters, the alert criteria, the recommended action and the flagged items. The leader analyzes every repository, artifact, image, account, token and the recent audit log every hour (`FORKLIFT_INSIGHTS_INTERVAL`) and ranks 25 kinds of improvement by severity, each with the affected items, links to where they are fixed, and who acts on it (DevOps and SRE, security, developer). A score out of 100 (for example 45 / 100) with a 30-day trend sits on top. Administrators, auditors and security reviewers see everything. Other users see the findings for repositories they can read. The API serves it at `GET /api/v1/insights`, and `POST /api/v1/insights/refresh` runs it on demand. See `docs/security-insights.md`.
- Security insight settings on the Security tab (Settings, admin or security): the analysis schedule and interval, and a scheduled alarm with its receiver, cron and timezone, minimum severity and an option to skip it when nothing is new. They are stored in the database and take effect within a minute, and `FORKLIFT_INSIGHTS_INTERVAL` becomes the default until they are saved. The API is `GET` and `PUT /api/v1/insights/settings`. The Security and Coverage tabs both name their settings button Settings.
- Every insight check has a stable code (SC supply chain, PO policy, EX exposure, AC access, OP operations, for example AC-05), shown in the list, on the check page and in alarms. `/workspace/insights/<code>` opens the check.
- Insight checks can be acknowledged, as in Trusted Advisor, by administrators and security reviewers with a required reason and a period of up to 365 days. An acknowledged check leaves the score, the counts, the badge and the alarms until the period ends or it flags more items than when acknowledged. The API is `PUT` and `DELETE /api/v1/insights/checks/{id}/acknowledgement`.
- Each insight run records every check's status and flagged count. A check page charts the last 30 days side by side, status over time and flagged items, for administrators, auditors and security reviewers. The API is `GET /api/v1/insights/checks/{id}/history`. The check page's Metrics zoom like Grafana: drag across a chart to filter both to that time range (kept in the URL as `from` and `to`), and Reset zoom to go back. They stay readable at thousands of runs (day labels that never overlap, and status buckets that keep the most urgent status).
- Security insight alarms: the Scheduled alarm section of the Security tab's Settings previews the message (rendered as Slack shows it by default, or as the unrendered mrkdwn source with syntax colouring and a copy icon) and sends the latest analysis at once with Send now, the same as the Coverage report. The API is `GET /api/v1/insights/notification/preview` and `POST /api/v1/insights/notification/send`.
- Every view is a shareable link: the Insights filters and each check's own page, the search and page of the Artifacts, Approvals and Coverage tables, the Approvals status and repository, and the Audit event filter live in the URL. Each insight check page adds a copy-link icon.
- Retention deletions record why in the audit entry's detail: the rule, the `idle_ttl` in force, the last activity, and for OCI the manifest digest.
- Idle retention covers OCI repositories. A tag neither pushed nor pulled within the repository's `retention.idle_ttl` is removed, and the reachability prune reclaims its manifest and layers. Cosign signature and attestation tags follow their image. Each removal is audited as `ttl.expire`.
- Repository Settings preview the impact of a retention value before it is saved: how many artifacts (or OCI tags) the next pass would delete, their size, and the least recently used of them with who uploaded each. A row links to the artifact (the Artifacts tab searched for its path) or to the OCI image. The API serves it at `GET /api/v1/repositories/{id}/retention-preview?days=N`.
- The Repositories list marks visibility with an icon, a globe for Public and a lock for Private.
- The Repositories list shows an hourglass badge with the period next to each repository whose idle retention is on.
- The artifacts table and the OCI image view show an Auto-delete column with the date each row becomes eligible for idle deletion. The listing APIs return it as `expires_at`.

### Changed

- Insights for a viewer limited to some repositories (a developer): the check list and statuses stay in view but dimmed and not clickable, and a check page opened by link says it is for administrators, auditors and security reviewers. Their score is labelled Security score for your repositories, and the figures under it count only their repositories, artifacts, images and the scan coverage of those, and their own access tokens (`inventory` in the API is narrowed the same way).
- The announcement is written on its own Admin > Announcement page instead of from a Write announcement button above every workspace page. Deleting all the text and saving takes the notice down, with no separate clear button. The page shows a live preview, the real bar redrawn as you type, above a Markdown editor with syntax colouring (a single Enter is a line break, no trailing spaces needed), and an optional end time picked on the console's calendar or typed (date and 24-hour time, browser time zone), after which the banner disappears on its own. The API takes and returns `expires_at` and reports `active`. The notice shows as one straight bar running edge to edge across the top of the page and pinned there while scrolling, on every menu except Admin, with an Announcement title, who posted it and how long ago (person and clock icons, the exact time on hover), over the whole Markdown, always in view (the bar grows with the notice). The end time is kept to the Admin page. As on GitHub Enterprise Server, users may dismiss the notice only when the administrator turns on Let users dismiss it (`user_dismissible`, off by default), and a dismissal lasts until the notice changes. This replaces the hide-for-7-days snooze. It only displays, for administrators too.
- A settings page that belongs to a parent (Security and Coverage Settings) returns to it after Save, the same as the receiver form. A failed save stays on the page with the error.
- Every copy control in the console is one minimal icon without a label: the copy glyph (or the link glyph for a page link), swapped for a green check for a moment once copied. This covers the repository name, endpoint and upstream, the new token, table cells, the alarm previews and the insight check link.
- Every Slack alarm (approval, grouped approval, test, coverage report and security insights) is also sent as Slack rich text: the webhook body adds a `blocks` field with one `rich_text` block, and numbered lines become a native ordered list. `text` keeps the same mrkdwn for Mattermost and for Slack notifications. The console's Slack preview renders the lists the same way, and the Coverage settings preview now has the same Slack (rendered) and Raw (unrendered mrkdwn with syntax colouring) tabs as the Security settings.
- Coverage moved under the new Insights menu as a tab next to Security. The sidebar badge now counts critical and high security findings instead of showing the coverage percentage.
- The retention setting in repository Settings is a switch, and the number of days (instead of a free-form duration) appears only while it is on. Values saved in other units through the API keep working.
- The Repositories table sizes every column to its content, removing the wide gaps between columns and the oversized Name column.

### Fixed

- The Repositories list and the Statistics tab count an OCI repository's images (tags) instead of every manifest and blob row, so five pushed images read as 5, not 15, everywhere in the console. Labeling coverage and the scanned total use the same image count. The `forklift_repository_artifacts` gauge still counts rows.

## 0.15.0 (2026-10-08)

Chart 0.15.0. forklift-mcp remains 0.3.2.

The 0.15.0 chart was rebuilt on 2026-10-08 so the bundled SeaweedFS skips its bucket hook Job by default (see Changed). The first 0.15.0 chart build renders the hook.

### Added

- Object-store health checks. Every pod sends a `HeadBucket` to the configured bucket once a minute (5s timeout) and keeps the last 60 results in memory. The Storage page draws them as a one-hour timeline with a time axis, uptime and average latency next to the cluster usage bar, and polls every 5 seconds regardless of the auto-refresh toggle. The admin API serves them at `GET /api/v1/storage/health`. The history is per pod and starts empty after a restart.
- `forklift_storage_up` and `forklift_storage_check_duration_seconds` gauges report the latest check, so availability history and alerting live in Prometheus. Both stay absent until the first check finishes. See `docs/metrics.md`.
- The Storage overview splits the backend into a Storage info card (official provider logo when one is bundled, name and server version from the admin API) and a Storage endpoint card with a copy button.
- The migration report records its own size: `stored_at.size_bytes` in `GET /api/v1/storage/migrations/{id}`, shown as Size under Report location.

### Changed

- A migration report opens as its own page at `/admin/storage/migrations/{id}` instead of a modal, so it can be linked, reloaded and opened in a new tab.
- The report's JSON tab shows the full record including `stored_at`, and the record path copies on hover.
- Hovering a run's check dots names the stage (PF Preflight, PV Postflight) above the per-check list.
- The bundled SeaweedFS no longer renders its bucket hook Job by default. `seaweedfs.allInOne.s3.createBuckets` defaults to `[]` and forklift's bucket falls back to `forklift`, which forklift and the migration Job already create at boot. Argo CD runs the hook as PostSync on every sync, and a hook pod that never starts (for example on a node out of pod IPs) keeps the sync waiting forever because the Job has no deadline. Set `createBuckets` only for bucket lock, versioning, TTL or anonymous read, which brings the hook back. A release that sets `createBuckets` explicitly keeps its hook and bucket name unchanged.

### Fixed

- The Checks column of the migration history no longer runs into the row's arrow when two or more checks failed. The failing IDs wrap below the dots.

### Security

- The web UI bundles seroval 1.6.8, patching GHSA-p6vx-979v-rg4c (critical) and its TypedArray memory exhaustion advisory. Dev-only dependencies are pinned to patched releases as well (proxy-addr, shell-quote, source-map-js, `@modelcontextprotocol/sdk`, joi, postcss-selector-parser). braces and sprintf-js have no patched release yet and are ignored in `pnpm audit` until one ships. Neither is bundled.

## 0.14.1 (2026-10-03)

Chart 0.14.1. forklift-mcp remains 0.3.2.

The 0.14.1 image was rebuilt on 2026-10-03 with the S3 timeouts, the migration progress logs and the log component of `log` crate events below. The first 0.14.1 build lacks them.

### Changed

- S3 requests time out: connecting after 10s and the response after `FORKLIFT_STORAGE_S3_READ_TIMEOUT` (default `10m`, upload body included), then retry. A request to a store that went away mid-flight used to wait forever, which stalled a migration Job whose target restarted during the snapshot upload. `migrate-storage` takes `FORKLIFT_MIGRATE_TO_S3_READ_TIMEOUT` for the target.
- `migrate-storage` logs every preflight and postflight check the moment it finishes, and announces the slow steps (the snapshot download, its integrity check, the blob checks and the re-hash) before they start, so a run that spends minutes on one step shows where it is.
- Events from dependencies that log through the `log` crate name their crate in `component` (for example `rustls`) instead of `log`, and drop the `log.target`, `log.module_path`, `log.file` and `log.line` fields.

### Fixed

- OCI proxies follow the upstream's redirect for a blob, dropping the registry's credentials once it leaves the registry's host. Docker Hub and ghcr serve layers from a CDN behind a 307, so every uncached layer failed with `502 upstream error` and images could not be pulled through `docker.io-proxy` or `ghcr.io-proxy`. Manifests were unaffected.
- The PyPI proxy's HTML index (`/simple/<project>/` without the PEP 691 JSON media type) lists every file again. pypi.org sends `"requires-python": null` for files without it, which failed the whole index and served an empty page. Clients that request JSON, such as pip 26, were unaffected.

## 0.14.0 (2026-10-03)

Chart 0.14.0. forklift-mcp remains 0.3.2.

The 0.14.0 image and chart were rebuilt on 2026-10-03 so the bundled SeaweedFS and the migration Job work under Argo CD: forklift and the migration Job create a missing bucket themselves, the migration Job is named by a hash of its settings instead of the Helm revision, every migration run is recorded and shown on the Storage page, the bundled SeaweedFS no longer restarts on every Argo CD sync, and every log line names its component. The first 0.14.0 build lacks all five.

### Removed

- The bundled MinIO subchart (`minio.*`). MinIO's images were removed from the public quay.io registry (`quay.io/minio/minio`), so the default install could no longer pull them. See [IBM's advisory](https://www.ibm.com/support/pages/node/7289585).

### Added

- A bundled SeaweedFS 4.48 subchart in all-in-one mode (`seaweedfs.*`), enabled by default and auto-wired to the s3 backend, admin endpoint included. The chart writes its S3 identities file (`forklift-seaweedfs-s3-config`, admin only) instead of the subchart's, whose random read-only key changed on every Argo CD render and restarted the SeaweedFS pod.
- Admin metrics for MinIO, RustFS, SeaweedFS and Garage behind one provider registry (`FORKLIFT_STORAGE_S3_PROVIDER`, `_ADMIN_ENDPOINT`, `_ADMIN_TOKEN`). The Storage page shows the provider by name and leaves counts a store does not report as unknown.
- `forklift migrate-storage` copies an object-storage deployment to another store: the blobs its metadata names, re-hashed on arrival, then the validated snapshot. Twelve preflight checks (PF01-PF12) run before the first write and are reported together with response times and a one-line summary. It holds the HA Lease while copying, refuses while a forklift pod exists, and has `--dry-run` and `--interactive`. See `docs/storage-migration.md`.
- Postflight integrity checks (PV01-PV04) after the copy: the uploaded snapshot hash, every named blob's size, a re-hash of a sample (or all, `--verify=full`) of the blobs starting with the ones skipped as present, and the source snapshot ETag. A failure deletes the bad target blobs and the target snapshot so a rerun copies them again.
- Migration history. Every `migrate-storage` run, succeeded, failed or dry, writes a record to `<prefix>/meta/migrations/` in the target. The Storage page lists them with provider logos, copy progress and per-check status, and opens a full report: the stage reached, failing checks and cleanup, timings, settings, every check with its latency, where the record is stored, and the raw JSON. The admin API serves them at `GET /api/v1/storage/migrations` and `GET /api/v1/storage/migrations/{id}`, and `GET /api/v1/storage` adds `last_migration`.
- Hovering a role on the Users page shows its description and how many users hold it.
- Every log line carries `component`, the module it comes from (`migrate`, `objstore`, `cluster`, `api` and so on, `main` for startup, the crate name for a dependency), after `level` in JSON and text alike.
- `storage.migration.*` runs that migration as a chart Job, keeping forklift at 0 replicas while enabled. The target is the chart's own storage, bundled SeaweedFS included.
- A startup probe for S3 conditional writes. HA refuses to start on a store that accepts writes whose precondition failed (Garage), since metadata fencing depends on them. The result is on the Storage page.

### Changed

- Boot waits up to 240s, with backoff, for an object store that is not reachable yet or whose bucket does not exist yet, instead of exiting into a restart loop. Credential errors still fail at once, and S3 errors now carry their root cause.
- A deliberate Lease release (shutdown, or the end of a migration) logs `released leadership` at INFO rather than `lost leadership` at WARN.
- The page itself no longer bounces at the scroll ends (`overscroll-behavior-y: none`), nor do the sidebar, dialogs, global search and select menus.
- Outside Kubernetes, PF01 lease reports `no HA Lease outside Kubernetes` instead of `not run: an earlier check failed`.
- Custom S3 endpoints no longer send aws-chunked checksum trailers, which Garage rejected as an invalid payload signature.
- `GET /api/v1/storage` replaces `mode`, `minio` and `minio_error` with `provider`, `provider_name`, `cluster` and `cluster_error`, and adds `conditional_writes`.

## 0.13.4 (2026-09-24)

Chart 0.13.2. forklift-mcp remains 0.3.2.

The 0.13.4 image was rebuilt on 2026-09-28 with the multipart upload fix and the stricter upload configuration below; the 2026-09-24 build lacks both.

The 0.13.3 image was rebuilt on 2026-09-23 with everything below except the crates.io download fix and the dependency upgrade.

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
- Every Rust dependency is on its latest release, 14 direct upgrades within their current major versions (among them reqwest 0.13.5, rustls 0.23.45, aws-sdk-s3 1.149.0 and rmcp 3.4.1) and the lockfile refreshed. rmcp's renamed `ServerConfig` replaces the deprecated `ServerInfo` alias in forklift-mcp's server, with no behaviour change.
- Malformed `FORKLIFT_UI_UPLOAD_*` values fail startup and name the offending variable instead of silently falling back to the default, so a typo in `FORKLIFT_UI_UPLOAD_ENABLED` no longer leaves uploads enabled. Validation also rejects `maxBatchBytes` below `maxFileBytes` or `goMaxZipBytes`. Upload configuration moved to `src/config/upload.rs`.

### Fixed

- A group's Cargo `config.json` pointed `dl` at its first member, so every `.crate` download bypassed the group and reached only that member. Crates held by later members, such as the `crates-io` proxy behind `cargo-public`, answered `404`. `dl` now names the group.
- A Cargo proxy of crates.io could not download any crate. It joined the download path onto the upstream index URL, but crates.io serves its index from `index.crates.io` and its crates from `static.crates.io`, so every `.crate` fetch answered `404` and every build pulling a crates.io dependency through forklift failed at the download step. The proxy now reads `dl` from the upstream `config.json` (cached for 15 minutes) and expands it the way cargo does, `{crate}`, `{version}`, `{prefix}`, `{lowerprefix}` and `{sha256-checksum}` included, with cargo's `/{crate}/{version}/download` suffix when there are no markers. A registry without a usable `config.json` keeps the previous behaviour. Upstream credentials are sent to the download host only when it is the index host.
- `GET api/v1/crates` on a group, which has no trailing slash, was classified as a sparse-index entry and aggregated. It now fans out like any other Web API call.
- Uploads over 2 MiB were stored truncated. The uploader rebuilt the request before parsing multipart, so axum's 2 MiB extractor default applied, and every chunk loop took the resulting read error as the end of the part. `npm publish` of a 2-3 MiB tarball failed with `422 npm_tarball_invalid`, and formats without archive checks could commit a truncated file. Every multipart body the uploader and the native npm, PyPI and Cargo adapters parse is now bounded by `uiUpload.maxBatchBytes` plus manifest and field allowances, a failed read answers `413 request_too_large` at the limit or `400 multipart_invalid` otherwise, and the partial blob is left to the sweeper.
- The hardcoded 256 MiB limits on raw browser uploads, legacy PyPI uploads and the npm publish document now derive from `uiUpload.maxFileBytes`. The npm document bound is the base64 size of `maxFileBytes` plus 64 MiB of metadata, about 405 MiB with the defaults. Size rejections name the configured limit in the IEC form the setting accepts, such as `256MiB`.

### Security

- The `/pattern/` regex search on the artifact and approval listings caps a pattern at 256 bytes and its compiled program at 1 MiB, a tenth of the regex crate's default. Matching was already linear-time, so this bounds the remaining cost, compiling an oversized pattern, per request.

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
