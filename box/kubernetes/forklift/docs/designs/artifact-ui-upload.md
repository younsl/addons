# Web UI artifact upload

## Status

**Implementation status: Implemented.** Verified on 2026-08-16 against `main`
commit `5db514b`. The management API endpoints live in `src/api/uploads.rs`
(`receiveUpload`, `commitUpload`, `cancelUpload`), the uploader facade in
`src/repo/uiupload.rs`, group metadata aggregation in
`src/repo/group_metadata.rs`, and the upload route in
`web/src/routes/workspace/repositories/$id/upload.tsx` with per-format forms for
Maven, npm, Cargo, Go, and PyPI.

Accepted for implementation. This document defines the complete v1 web upload
contract for hosted repositories and is based on `main` commit `1c9d238`.
The format contracts were revalidated on 2026-07-22 against current official
Maven, npm, Cargo, Go, and PyPA specifications and against Forklift's native
protocol/group handlers; the mutation and group-aggregation rules below include
the resulting corrections.
Normative sections use **must** semantics even where explanatory prose says
“should”; implementation changes to API, persistence, collision, format, or
security behavior require updating this design before merge.

## Overview

This design defines the v1 contract for uploading artifacts to a hosted
repository from the web UI and the management API: the request shape, the
per-format validation rules, collision handling, and the permissions each
mutation requires.

Read this before changing upload behavior in the API, the persistence layer, or
any format handler.

## Background

Each packaging ecosystem publishes differently. Maven writes a series of
individual files, npm and PyPI accept a single archive over a defined publish
API, Cargo publishes only through its own registry API, and Go has no publish
command at all. A repository manager that offers one upload experience has to
absorb those differences and still leave every repository in a state the native
clients can read.

Two rules follow. A publication is a set of files plus the index entries derived
from them, and it must land atomically, because a half-written index breaks
resolution for every consumer. And the coordinate a user types must be validated
against the metadata inside the archive, because the two disagreeing is the
common way a repository ends up serving a package under the wrong name.

## Decision summary

Forklift should add a contextual **Upload artifact** action to the Artifacts tab
of a hosted repository. The action opens a dedicated upload route whose form is
specialized for the repository format. The browser sends one streaming
`multipart/form-data` request to the management API; a new uploader facade in
`src/repo` validates the package, derives canonical repository paths, and
commits all generated artifacts through the existing content-addressed storage,
audit, metrics, vulnerability-scan, and license-resolution pipeline.

The feature is complete only when every format Forklift currently exposes —
Maven, npm, Cargo, Go, and PyPI — is supported. Implementation may proceed as
independently reviewable vertical slices, but the public UI-upload feature must
not be declared generally available while some hosted formats still lead to a
dead end. A generic path-and-file fallback is deliberately excluded: it is easy
to build but can create packages that appear uploaded while remaining unusable
by their native clients.

## Nexus Repository Community reference

[Nexus Repository Community](https://github.com/sonatype/nexus-public) already
provides UI upload for hosted repositories. The useful product and
implementation patterns are:

- The upload action is available only for online hosted repositories and users
  with component-upload plus repository edit privileges.
- The UI selects a hosted repository, presents format-specific fields, accepts
  one or more files where the format permits it, and redirects to the uploaded
  component after success.
- Maven accepts multiple assets and coordinates; when a POM is present, Nexus
  can extract coordinates from it. It can also generate a POM from entered
  coordinates.
- The current React UI obtains an upload definition from the server, builds a
  dynamic form, validates required and unique asset fields, creates
  `FormData`, and submits it to a multipart upload endpoint.
- The server verifies hosted/online state, selects a format-specific upload
  handler, parses multipart data, validates the component, persists it, emits
  an upload event, and closes every uploaded payload.

References:

- [Uploading Components](https://help.sonatype.com/en/uploading-components.html)
- [Components API](https://help.sonatype.com/en/components-api.html)
- [Nexus React upload form](https://github.com/sonatype/nexus-public/blob/0a8a425daa4b37e924ca11e4637a41afce7b115c/public/common/components/nexus-coreui-plugin/src/frontend/src/components/pages/browse/Upload/UploadDetails.jsx)
- [Nexus React upload state and multipart submission](https://github.com/sonatype/nexus-public/blob/0a8a425daa4b37e924ca11e4637a41afce7b115c/public/common/components/nexus-coreui-plugin/src/frontend/src/components/pages/browse/Upload/UploadDetailsMachine.js)
- [Nexus upload manager](https://github.com/sonatype/nexus-public/blob/0a8a425daa4b37e924ca11e4637a41afce7b115c/public/common/components/nexus-repository-services/src/main/java/org/sonatype/nexus/repository/upload/internal/UploadManagerImpl.java)
- [Maven repository layout](https://maven.apache.org/repositories/layout.html)
- [Maven repository metadata](https://maven.apache.org/repositories/metadata.html)
- [npm package metadata](https://docs.npmjs.com/files/package.json/)
- [npm publish immutability](https://docs.npmjs.com/cli/publish/)
- [npm dist-tag rules](https://docs.npmjs.com/cli/dist-tag/)
- [Cargo registry index](https://doc.rust-lang.org/cargo/reference/registry-index.html)
- [Cargo registry web API](https://doc.rust-lang.org/cargo/reference/registry-web-api.html)
- [Go module proxy and zip specification](https://go.dev/ref/mod)
- [Python wheel specification](https://packaging.python.org/en/latest/specifications/binary-distribution-format/)
- [Python source distribution specification](https://packaging.python.org/en/latest/specifications/source-distribution-format/)
- [Python core metadata](https://packaging.python.org/en/latest/specifications/core-metadata/)
- [Python Simple Repository API](https://packaging.python.org/en/latest/specifications/simple-repository-api/)

Forklift should reuse these product ideas, not source code. Nexus files carry
their own license notices and some legacy UI code has additional Ext JS terms;
Forklift's implementation must be written independently under this project's
Apache-2.0 license.

## Goals

- Let an authorized user publish a valid package from the repository's
  Artifacts tab without learning the native publish command first.
- Produce content that Maven, npm, Cargo, Go, and PyPI clients can consume, not
  merely store an arbitrary file.
- Apply exactly the same repository state, RBAC, source-IP ACL, storage,
  deduplication, scanning, audit, and observability semantics as native client
  uploads.
- Stream files with bounded memory and no dependency on writable container
  temporary storage.
- Make collisions, validation failures, partial failures, and successful paths
  explicit to the user.
- Work on desktop and mobile and remain usable with keyboard and screen reader.

## Non-goals

- Replacing Maven, npm, Cargo, Go, `twine`, or CI/CD publishing workflows.
- Uploading to proxy or group repositories.
- Bulk directory import or repository migration.
- A raw repository format; Forklift does not currently expose one.
- Browser-side archive parsing as a source of truth.
- Globally changing overwrite semantics for legacy raw Maven/Cargo/Go PUT paths.
  Native npm and PyPI publish must adopt the same publication immutability rules,
  and every raw handler must refuse writes to UI-managed paths; otherwise a
  native request could invalidate the lifecycle guarantees in this design.
- A global Upload navigation item; v1 entry is contextual to one repository.
- Resumable/chunked upload sessions; v1 retries use idempotency and retained
  conflict blobs but transfer failures restart the request.
- Repository-configurable redeploy policies; v1 uses the fixed per-format
  mutation table in this document.

## Runtime configuration

Add a first-class upload section to `config.Config`. These are process-level
resource and rollout controls, not repository policy:

Upload limits are represented by `UploadConfig` in `src/config.rs`; the table below defines the resource controls.

| Environment / flag | Development default | GA default | Constraint |
| --- | ---: | ---: | --- |
| `FORKLIFT_UI_UPLOAD_ENABLED` / `--ui-upload-enabled` | `true` | `true` | Global capability and route gate; set `false` to opt out |
| `FORKLIFT_UI_UPLOAD_MAX_DURATION` / `--ui-upload-max-duration` | `30m` | `30m` | `1m..2h` |
| `FORKLIFT_UI_UPLOAD_MAX_CONCURRENT` / `--ui-upload-max-concurrent` | `4` | `4` | `1..32` process-wide |
| `FORKLIFT_UI_UPLOAD_MAX_CONCURRENT_USER` / `--ui-upload-max-concurrent-user` | `2` | `2` | `1..8`, not above global |
| `FORKLIFT_UI_UPLOAD_MAX_ASSETS` / `--ui-upload-max-assets` | `16` | `16` | `1..64` |
| `FORKLIFT_UI_UPLOAD_MAX_FILE_BYTES` / `--ui-upload-max-file-bytes` | `256MiB` | `256MiB` | `1MiB..1GiB`; non-Go default |
| `FORKLIFT_UI_UPLOAD_MAX_BATCH_BYTES` / `--ui-upload-max-batch-bytes` | `512MiB` | `512MiB` | Must be at least max-file |
| `FORKLIFT_UI_UPLOAD_GO_MAX_ZIP_BYTES` / `--ui-upload-go-max-zip-bytes` | `500MiB` | `500MiB` | Cannot exceed Go's 500 MiB protocol limit |

Byte-valued settings accept a positive integer byte count or case-insensitive
IEC suffixes `KiB`, `MiB`, and `GiB`; values are normalized to `int64` during
configuration validation. Invalid combinations fail process startup rather than
silently falling back.

The remaining bounds are deliberately not public tuning knobs in v1:
manifest 64 KiB, non-file fields 1 MiB total, 100,000 archive entries, 16 MiB
of metadata read from any one archive, and a 24-hour idempotency TTL. Keeping
security-sensitive parser limits fixed prevents an operator from accidentally
disabling bomb protection. A later release may expose them only with hard
ceilings.

When disabled, repository capabilities report `upload=false`, the UI has no
entry point, and POST returns 404 so a disabled experimental surface is not
advertised. Existing native publish endpoints are unaffected.

Add one repository-level compatibility setting to `repoconfig.Config`:

```json
{"upload":{"pypi_allow_legacy_zip":false}}
```

It is valid only for hosted PyPI repositories, defaults false, and is edited by
administrators in repository Settings. It affects UI validation and the upload
handler, not native `twine` behavior. No other per-repository upload setting is
introduced in v1.

## UX design

### Native publisher compatibility

UI upload is an additional publication surface, not a claim that every
ecosystem has the same native publish protocol. The supported matrix is
normative for v1:

| Format | Ecosystem publisher | Forklift before this feature | v1 commitment |
| --- | --- | --- | --- |
| Maven | `mvn deploy` / `mvn deploy:deploy-file` | Supported through repository-path PUTs | Preserve CLI deployment; UI provides atomic release publication. Native multi-request deploy remains legacy/raw and cannot claim UI transaction atomicity |
| npm | `npm publish` / compatible clients | Supported through npm's packument PUT | Preserve it and refactor it onto the same publication/immutability service as UI upload |
| PyPI | `twine upload` and clients using the legacy multipart upload API | Supported at the hosted repository root | Preserve it and refactor each uploaded distribution onto the same validation/publication service; sequential wheels extend one release |
| Cargo | `cargo publish` using the Registry Web API | Not supported; generic path PUT is not Cargo's publish API | UI upload is supported, `cargo publish` remains explicitly unavailable, and `config.json` omits `api` |
| Go | No standard module publish command; proxies normally ingest from VCS/module cache | Generic GOPROXY-path PUT only | UI upload is supported; do not present raw PUT as an ecosystem-native publisher |

The repository DTO adds `publish_methods`, computed by format and rollout state:

```json
[
  {"id":"ui","available":true},
  {"id":"native","available":true,"client":"npm"}
]
```

The upload page contains a collapsed **Prefer the command line?** panel. Maven,
npm, and PyPI show copyable commands plus links to credential setup. Cargo says
that `cargo publish` is not implemented and directs the user to UI/API upload.
Go explains that the ecosystem has no standard publish command and directs the
user to UI/API upload. Examples reference repository URLs and environment/config
variable names but never interpolate passwords, PATs, session cookies, or CSRF
tokens into DOM text or clipboard content.

Native npm and PyPI handlers must call the same format publication planner and
`ApplyArtifactBatch` used by UI upload; they differ only in request decoding and
response shape. Their conflicts, tombstones, validation, scan/license queues,
audit attribution, and group-cache invalidation must therefore be identical.
Maven's native deployment consists of independent PUT requests with no portable
end-of-component marker, so it stays path-oriented. Raw Cargo/Go/Maven PUTs are
documented as advanced compatibility APIs, not advertised as native publish
commands, and may not mutate UI-managed paths.

### Entry and visibility

The Artifacts card header gains a primary **Upload artifact** button beside the
item count.

The server computes repository capabilities for the current principal:

```json
{
  "capabilities": {
    "read": true,
    "write": true,
    "delete": false,
    "upload": true
  }
}
```

`upload` is true only when the repository is `hosted`, is not disabled, supports
UI upload for its format, and the principal can perform `write`. The client must
not infer this from `me.admin`; scoped writers are valid uploaders. The API
still enforces every condition because UI visibility is not authorization.

For proxy and group repositories the button is absent. The existing endpoint
card remains unchanged. When its artifact list is empty, render the translated
hint “Upload packages to a hosted member repository.” For a disabled hosted
repository, show the disabled state but no actionable upload control.

### Route and layout

Use a dedicated route:

```text
/workspace/repositories/{id}/upload
```

A modal is too cramped for Maven multi-asset forms, server errors, mobile
keyboards, and upload progress. The page keeps repository context in its header:
repository name, `hosted`, format badge, and a back link to Artifacts.

The page has three visual regions:

1. **Files** — drag-and-drop zone and file picker, accepted extensions, size
   limits, file name, size, and remove action.
2. **Package details** — format-specific fields. Values inferred by the server
   are confirmed in the result; user-entered values are treated as claims and
   validated against package metadata where metadata exists.
3. **Review** — target repository, coordinate, files, collision policy, and the
   final Upload button.

This is one page, not a blocking wizard. Sections progressively appear as files
are selected, which keeps simple PyPI and npm uploads short while allowing Maven
to expose multiple assets.

### Interaction states

- **Idle:** accepted types and a native-client publishing hint are visible.
- **Ready:** client-side checks cover missing files, empty required fields,
  duplicate Maven asset roles, per-file size, total size, and extension.
- **Uploading:** inputs are locked; each request shows byte progress, transferred
  size, and a Cancel action. Use `XMLHttpRequest.upload.onprogress` because the
  current Fetch API path does not provide reliable request upload progress.
- **Conflict:** HTTP 409 lists exact existing paths and the format-owned
  recovery action. Maven may offer **Replace existing artifacts** to a user
  with `delete`. PyPI automatically extends an existing release only when every
  submitted filename is new; a filename collision is rejected. npm, Cargo, and
  Go never offer replacement because their client protocols treat published
  version bytes as immutable. Retained staging is reused only when the returned
  `conflict_action` is actionable.
- **Success:** show the canonical coordinate, created/replaced paths, total
  bytes, scan status as queued, and actions for **View artifacts** and **Upload
  another**. Refresh the Artifacts list when returning.
- **Failure:** keep selected files and fields when the browser still owns them,
  focus a summary alert, and map structured field errors next to their inputs.

Cancel means aborting the HTTP request. The server must notice context
cancellation, abandon any unreferenced staged blobs, and never expose a partial
component.

### Screen contract and wireframes

Artifacts-tab entry, desktop:

```text
┌ Artifacts                                      128 items · 1.4 GiB ──────┐
│ [Filter by path prefix________________] [Filter]       [Upload artifact] │
│ Path                     Version   Vulnerability   License   Size   ...  │
│ ...                                                                    │
└─────────────────────────────────────────────────────────────────────────┘
```

Upload page, desktop:

```text
‹ Back to artifacts
Upload artifact       [hosted] [maven]                 maven-hosted
Publish a release that Maven clients can resolve from this repository.

┌ 1. Files ───────────────────────────────────────────────────────────────┐
│ Drop files here or [Choose files]       JAR, POM, WAR, ZIP · 256 MiB ea │
│ widget.jar       38.1 KiB  extension [jar] classifier [          ] [×] │
│ sources.jar      12.0 KiB  extension [jar] classifier [sources   ] [×] │
│ [+ Add another asset]                                                │
└────────────────────────────────────────────────────────────────────────┘
┌ 2. Package details ────────────────────────────────────────────────────┐
│ Group ID [com.acme____] Artifact ID [widget____] Version [1.4.0____] │
│ [✓] Generate a POM                 Packaging [jar_______________]     │
└────────────────────────────────────────────────────────────────────────┘
┌ 3. Review ─────────────────────────────────────────────────────────────┐
│ Target     maven-hosted                                              │
│ Coordinate com.acme:widget:1.4.0                                     │
│ Creates    3 package files + 8 generated metadata/checksum files      │
│                                        [Cancel] [Upload artifact]      │
└────────────────────────────────────────────────────────────────────────┘
```

During transfer, the Review card becomes a progress region with one aggregate
bar, bytes/total, elapsed time, and **Cancel upload**. Do not fake per-file
progress because multipart transport exposes aggregate request progress only.

Conflict replaces Review with a warning card listing at most five paths and an
expandable remainder. The button and explanation come from the server's
`conflict_action`: `replace` shows an unchecked destructive acknowledgement and
**Replace version**; `reject` explains the ecosystem's immutability rule and
offers only **Back to files**. The client never infers this solely from
`delete`.

Mobile at `<640px` stacks all fields and makes the bottom action bar sticky:

```text
┌──────────────────────────────┐
│ ‹ Artifacts                  │
│ Upload artifact             │
│ maven-hosted  hosted maven  │
│ ┌ Files ──────────────────┐ │
│ │ [Choose files]          │ │
│ │ widget.jar              │ │
│ │ Extension [jar_______]  │ │
│ │ Classifier [__________] │ │
│ └─────────────────────────┘ │
│ ┌ Package details ... ────┐ │
│ └─────────────────────────┘ │
│ ┌ Review ... ─────────────┐ │
│ └─────────────────────────┘ │
├──────────────────────────────┤
│ [Cancel] [Upload artifact]   │
└──────────────────────────────┘
```

The sticky bar must not cover focused inputs; use bottom padding equal to its
measured height. At `prefers-reduced-motion`, progress and section appearance
use no animated transitions.

### Per-format field contract

| Format | Files section | Package-details section | Client-side validation |
| --- | --- | --- | --- |
| Maven | Repeatable assets with extension/classifier; POM auto-recognized | GAV, Generate POM, Packaging | Unique extension/classifier, release-only version, POM vs generate requirement |
| PyPI | Repeatable wheel/sdist files | Read-only note that identity is extracted; Settings link when legacy ZIP compatibility is enabled | Supported suffixes, unique filenames, batch size |
| npm | Single `.tgz` | Dist tag, default `latest`; identity shown after server success | One file, `.tgz`, valid dist-tag token |
| Cargo | Single `.crate` | No editable identity; extraction notice | One file and `.crate` |
| Go | Single `.zip`, optional `.mod` | Module path, version; release time visible/editable only for admin import | Required module/version, `.zip`, at most one `.mod` |

The browser does not parse package archives. For npm, Cargo, and PyPI the
coordinate is therefore shown as “Read from package during upload” before
submission and as an exact value on success or validation failure. This avoids
loading large archives twice and keeps the server authoritative.

### Format-specific experience

| Format | UI inputs | Server-derived/validated output | Support commitment |
| --- | --- | --- | --- |
| Maven | One or more assets; Group ID, Artifact ID, Version; per-asset extension and optional classifier; optional Generate POM | Canonical GAV paths; POM coordinates when a POM is included; generated POM when requested; consistent file names, checksums, and release metadata | Required |
| PyPI | One or more distributions of the same release (`.whl`, `.tar.gz`; legacy `.zip` only behind an explicit compatibility option) | Normalized project name, version and filenames from wheel `METADATA` or source distribution `PKG-INFO`; `packages/{name}/{filename}` | Required |
| npm | One `.tgz`; optional dist-tag, default `latest` | `name` and `version` from `package/package.json`; canonical tarball path; merged packument preserving all existing versions and tags | Required |
| Cargo | One `.crate` | Name/version/dependencies from normalized `Cargo.toml`; SHA-256 checksum; crate download path; merged sparse-index entry | Required |
| Go | Module path, canonical version, one module `.zip`, and optional separate `go.mod` | Cross-validated zip root and module directive; canonical `.zip`, `.mod`, `.info`, merged `list`, and latest resolution | Required |

### Format mutation policy

The common transaction machinery does not imply a common redeploy policy.
Handlers return a server-owned mutation policy, and repository/publication DTOs
expose only the actions valid for that format:

| Format | Same-version upload | User-visible lifecycle action | Reason |
| --- | --- | --- | --- |
| Maven release | Replace complete publication with `write+delete` confirmation | Delete version | Private Maven repositories commonly allow controlled redeploy; metadata is rebuilt atomically |
| PyPI | Extend an existing release with new filenames; an existing filename is never replaced | Delete release; deleted filenames remain tombstoned | Installers cache distribution URLs and hashes, while one release commonly gains wheels over time |
| npm | Reject forever after the first successful publish, including after unpublish | Unpublish version; keep coordinate tombstone | npm defines a name/version pair as non-reusable |
| Cargo | Reject; SemVer identity also ignores build metadata | Yank/unyank; retain index line and crate bytes | Cargo index entries are immutable except `yanked` |
| Go | Reject | No delete or replace action | GOPROXY requires successful `.mod` and `.zip` responses to remain byte-identical |

The publication DTO includes `actions`, a subset of `replace`, `extend`,
`delete`, `yank`, and `unyank`, computed from format, state, and the current
principal. The UI renders only these actions. Administrative raw purge is an
out-of-band repair operation and is not part of v1 UI upload lifecycle.

### Hosted-first group metadata aggregation

The current generic group middleware serves the first non-404 member response.
That remains correct for immutable version-specific files, but is incorrect for
mutable package indexes: a hosted member containing one UI-uploaded version
would hide the proxy member's other versions. General availability therefore
also requires format-aware aggregation for these group request classes:

| Format | Aggregated group paths | Merge rule |
| --- | --- | --- |
| Maven | G-level/A-level `maven-metadata.xml` and requested checksum sidecars | Union plugins/versions in member order; first member wins duplicate prefix/version; first surviving member value supplies `latest`/`release`; regenerate timestamps/checksums |
| npm | Package packument | Union versions by canonical version with first-member precedence; union `time`; first member's tag wins and later members fill absent tags; rewrite every tarball URL to the group |
| Cargo | `config.json` and sparse crate index | Synthesize download URL/auth for the group itself; union lines by build-insensitive SemVer identity with first-member precedence; preserve member and within-file order |
| Go | `@v/list` and `@latest` | Union and semver-sort tagged list values; select latest from all valid member candidates using release, prerelease, then pseudo-time precedence |
| PyPI | `/simple/` and `/simple/{project}/` | Union projects and distribution filenames with first-member precedence; union versions; rewrite file URLs to the group |

Version-specific POM/JAR/wheel/tarball/crate/Go `.info`/`.mod`/`.zip` requests
continue ordinary member-order fallback, so bytes and collision precedence stay
predictable. Aggregate handlers execute after group authorization/IP ACL and
apply every member's security/age/approval filtering before merge. A 404 is a
miss; a member 401/403 or malformed metadata is authoritative and aborts rather
than leaking a later member. An upstream 429, timeout, connection failure,
truncated response, or 5xx is transient and returns 503 instead of serving a
silently incomplete index; forward the shortest valid `Retry-After` when one is
available. If every member returns 404, the group returns 404. An upstream 304
is usable only with the previously validated cached source bytes for that exact
member/path/representation and validator; a validator without source bytes is a
502 rather than an empty contribution.

Parsing uses the existing bounded format parsers and global rewrite semaphore,
with 64 MiB per member and 64 MiB final metadata limits. One singleflight per
`(groupID, path, representation)` prevents duplicate work. Cache the final
bytes in the CAS plus a `group_metadata_cache` row containing member repo IDs,
source artifact digests/validators, group-config revision, representation,
digest, and expiry. Managed local commits/deletes/yanks synchronously invalidate
affected group keys through reverse group membership; proxy TTL expiry makes
the cache stale. Responses send the aggregate digest as a strong ETag and honor
`If-None-Match`. Cached group metadata is internal and excluded from artifact
lists, scans, publication ownership, and user storage totals.

Native writes must use the same invalidation primitive. Refactor
`meta.PutArtifact`/`DeleteArtifact` call sites for mutable Maven metadata, npm
packuments, Cargo index files, Go list/latest, and PyPI distributions to delete
the corresponding group-cache rows in the same
[SQLite](https://github.com/sqlite/sqlite) transaction as the artifact mutation.
A short post-commit invalidation window is not acceptable.

The server rejects a mismatch between entered coordinates, archive metadata,
and naming conventions. It must not silently rewrite a user claim into a
different package identity.

Every format must satisfy a native-client contract before it is enabled in the
UI. A successful HTTP response is insufficient: the corresponding Maven, npm,
Cargo, Go, or pip client must be able to resolve and install the uploaded
version from both the hosted repository and its hosted-first group.

## API contract

### Repository capabilities

Add `capabilities` and `publish_methods` to repository detail and list DTOs.
Values are derived from the current principal, repository state, format, and
feature flag, so they must not be stored in SQLite. At minimum expose `read`,
`write`, `delete`, and `upload`; `publish_methods` follows the native publisher
matrix above and never reports generic raw PUT as an ecosystem-native client.

This also fixes a current UI limitation: the detail page knows only global
`admin`, `approver`, and `auditor` flags and therefore cannot render a
repository-scoped writer correctly.

Extend artifact-list items with nullable `publication_id`, `artifact_role`, and
`coordinate`. Return a separate `publications` array and keep the existing flat
`artifacts` array for compatibility:

```json
{
  "publications": [
    {
      "id": "01K...",
      "format": "maven",
      "coordinate": "com.acme:widget:1.4.0",
      "package": "com.acme:widget",
      "version": "1.4.0",
      "asset_count": 5,
      "total_size": 50549,
      "actions": ["replace", "delete"],
      "created_by": "alice",
      "created_at": "2026-07-22T12:00:00Z"
    }
  ]
}
```

The UI uses `publications` for grouped rows and server-provided lifecycle
actions; legacy/raw artifacts remain in the flat ungrouped section. Read
authorization for both is identical to the current artifact list.
`publications` contains exactly the IDs
referenced by the returned (currently maximum 500) artifact rows after prefix
filtering; it is not a second independently paged collection.

### Upload endpoint

```http
POST /api/v1/repositories/{id}/artifacts
Content-Type: multipart/form-data
```

Keep GET for listing and DELETE for deletion on the same resource. The POST is
authenticated and performs repository-scoped `write` authorization. Maven
`overwrite=true` additionally requires repository-scoped `delete`; the other
formats reject that field value.

Multipart parts:

- `manifest`: required UTF-8 JSON part, maximum 64 KiB.
- `asset0` ... `asset15`: streamed file parts referenced by the manifest.

Common manifest shape:

```json
{
  "schema_version": 1,
  "format": "maven",
  "overwrite": false,
  "assets": [
    {"part": "asset0", "extension": "jar", "classifier": ""},
    {"part": "asset1", "extension": "jar", "classifier": "sources"}
  ],
  "maven": {
    "group_id": "com.acme",
    "artifact_id": "widget",
    "version": "1.4.0",
    "generate_pom": true,
    "packaging": "jar"
  }
}
```

The `format` must equal the target repository format. Format objects are a
tagged union in OpenAPI, not a free-form map. File fields use stable part names
instead of trusting client filenames as form keys.

The other manifest variants are equally explicit:

```json
{
  "schema_version": 1,
  "format": "pypi",
  "overwrite": false,
  "assets": [
    {"part": "asset0"},
    {"part": "asset1"}
  ],
  "pypi": {}
}
```

All PyPI assets in one request must resolve to the same normalized project name
and version. This permits an sdist and several platform wheels to be published
as one atomic release.

```json
{
  "schema_version": 1,
  "format": "npm",
  "overwrite": false,
  "assets": [{"part": "asset0"}],
  "npm": {"dist_tag": "latest"}
}
```

The npm package name and version are not accepted as independent user input;
the tarball's `package/package.json` is authoritative. The result page displays
the extracted identity.

```json
{
  "schema_version": 1,
  "format": "cargo",
  "overwrite": false,
  "assets": [{"part": "asset0"}],
  "cargo": {"yanked": false}
}
```

UI upload publishes a non-yanked version only. The field remains in the
versioned contract for a separately designed administrative import; this
endpoint rejects `yanked:true` in v1.

```json
{
  "schema_version": 1,
  "format": "go",
  "overwrite": false,
  "assets": [
    {"part": "asset0", "role": "zip"},
    {"part": "asset1", "role": "mod"}
  ],
  "go": {
    "module": "example.com/acme/widget",
    "version": "v1.4.0",
    "time": "2026-07-22T12:00:00Z"
  }
}
```

The `mod` asset is optional. If it is absent, the server reads the top-level
`go.mod` from the zip; if neither exists, it creates the protocol-permitted
synthetic `module <path>` file. `time` defaults to server time and may be
supplied only by an administrator so ordinary uploaders cannot forge release
chronology.

Successful response (artifact arrays abbreviated; the real response lists every
publication-owned and derived path):

```http
HTTP/1.1 201 Created
```

```json
{
  "upload_id": "01K...",
  "repository": "maven-hosted",
  "format": "maven",
  "coordinate": "com.acme:widget:1.4.0",
  "created": [
    {"path": "com/acme/widget/1.4.0/widget-1.4.0.jar", "role": "primary", "size": 38124, "sha256": "a1..."},
    {"path": "com/acme/widget/1.4.0/widget-1.4.0-sources.jar", "role": "primary", "size": 12003, "sha256": "b2..."},
    {"path": "com/acme/widget/1.4.0/widget-1.4.0.pom", "role": "metadata", "size": 422, "sha256": "c3..."}
  ],
  "replaced": [],
  "derived": [
    {"path": "com/acme/widget/maven-metadata.xml", "role": "index", "size": 388, "sha256": "..."}
  ],
  "scan_status": "queued",
  "durability": "local",
  "warnings": []
}
```

Errors use a consistent problem response:

```json
{
  "type": "https://forklift.dev/problems/artifact-conflict",
  "title": "Artifact already exists",
  "status": 409,
  "code": "artifact_conflict",
  "detail": "2 target paths already exist",
  "field_errors": {},
  "conflicts": ["com/acme/widget/1.4.0/widget-1.4.0.jar"],
  "removes": [],
  "conflict_action": "replace",
  "confirm_url": "/api/v1/repositories/7/uploads/01K.../commit",
  "expires_at": "2026-07-22T13:00:00Z"
}
```

Required status semantics:

- `400` malformed multipart or manifest.
- `401` unauthenticated.
- `403` RBAC or source-IP ACL denied.
- `404` repository not found or not readable.
- `409` target collision or concurrent publication conflict.
- `413` file, request, or asset-count limit exceeded.
- `415` extension/content type unsupported for the format.
- `422` package metadata, coordinate, or archive validation failed.
- `429` global or per-principal upload concurrency exhausted; include
  `Retry-After: 5`.
- `503` repository disabled or storage unavailable.

Do not expose internal filesystem, object-store, SQL, or archive parser errors.

### Request ordering and headers

The UI endpoint has one canonical wire contract:

- `Idempotency-Key` is required. It is 16–128 printable ASCII characters and
  the browser generates a UUID v4 once per explicit upload attempt.
- `X-CSRF-Token` is required for session-cookie authentication. It is omitted
  only when an `Authorization` header authenticated the request.
- The `manifest` part must be first. This lets the server select limits and the
  format handler before accepting hundreds of MiB of untrusted content.
- File parts then appear in the same order as `manifest.assets`; every declared
  part appears exactly once and no undeclared file or field is allowed.
- Each file part must have a non-empty sanitized filename and
  `Content-Type: application/octet-stream`; declared browser MIME types are not
  used for validation.
- The request must end immediately after the last declared part. Duplicate
  names, an early EOF, trailing fields, or a second manifest return 400.

This strict order is intentional. It keeps the streaming parser simple,
deterministic, fuzzable, and independent of disk-backed multipart buffering.

### Idempotency contract

Add an `artifact_upload_requests` table:

```sql
CREATE TABLE artifact_upload_requests (
  idempotency_key TEXT NOT NULL,
  repo_id         INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  principal_name  TEXT NOT NULL,
  principal_source TEXT NOT NULL,
  upload_id       TEXT NOT NULL,
  state           TEXT NOT NULL CHECK(state IN ('receiving','conflict','committed','failed')),
  plan_json       TEXT NOT NULL DEFAULT '',
  result_json     TEXT NOT NULL DEFAULT '',
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  expires_at      TEXT NOT NULL,
  PRIMARY KEY (repo_id, principal_source, principal_name, idempotency_key)
);
CREATE UNIQUE INDEX idx_artifact_upload_id
  ON artifact_upload_requests(upload_id);

CREATE TABLE artifact_upload_staged_blobs (
  upload_id TEXT NOT NULL REFERENCES artifact_upload_requests(upload_id) ON DELETE CASCADE,
  sha256    TEXT NOT NULL,
  size      INTEGER NOT NULL,
  PRIMARY KEY(upload_id, sha256)
);
```

Behavior:

1. Before reading multipart bytes, insert `receiving`, scoped by repository and
   the principal's source plus username. A live duplicate returns
   409 `upload_in_progress` with its `upload_id`.
2. A duplicate whose state is `conflict` replays the same 409 plan; one whose
   state is `committed` returns the persisted result as 200
   with `Idempotency-Replayed: true` and drains or closes the new request body.
3. `failed` and stale `receiving` rows return 409 `idempotency_key_consumed`;
   retry requires a new key. This avoids accidentally binding different bytes
   to an old key.
4. The artifact batch and transition to `committed` occur in the same SQLite
   transaction. A process crash cannot leave committed artifacts paired with a
   `receiving` request.
5. Validation, cancellation, or storage failure marks the row `failed`
   best-effort. A leader-only cleanup task expires failed/committed rows after
   24 hours, expires conflict plans after 30 minutes, and converts `receiving`
   rows older than `MaxDuration + 5m` to `failed` before expiry.

Only an actionable Maven immutable conflict is retained: persist `plan_json`
capped at 1 MiB, insert each staged digest in
`artifact_upload_staged_blobs`, ensure those digests exist in the `blobs` table
with ref count zero, and transition to `conflict`. The sweeper excludes leased
zero-reference blobs until plan expiry. PyPI filename reuse and npm/Cargo/Go
immutable-version conflicts transition to `failed`, release leases, abandon
staged blobs, and return their terminal 409 immediately.

Maven confirmation uses:

```http
POST /api/v1/repositories/{id}/uploads/{uploadID}/commit
Content-Type: application/json

{"overwrite":true}
```

This confirmation operation exists only for a Maven `replace` plan. It requires
the original principal, `write` plus `delete`, CSRF, matching repository, an
unexpired `conflict` state, and current leader/ACL checks. It reopens the staged
primary blobs, replans all mutable metadata against current state, verifies
conflicts still belong to the same package/version, and commits.
No multipart bytes are transferred again. `DELETE` on the same upload URL
cancels the conflict plan and releases its leases. Expiration does the same.

The browser always sends `overwrite=false` on receive. A non-browser caller may
send `overwrite=true` only for Maven; the server rejects it for PyPI, npm,
Cargo, and Go with 422 `redeploy_not_supported` before reading file parts. Maven
requires `delete` before reading file parts and commits a same-publication
replacement directly after validation. It still refuses raw/unowned path
collisions and records removed paths in the result.

The idempotency record does not make asynchronous HA replication synchronous.
In S3 and replicated-PV modes, a failover inside the configured metadata-sync
interval can lose both artifact rows and the result record, consistent with the
platform's documented RPO. Content-addressed blob bytes may survive as harmless
orphans and are reclaimed later. The success response includes
`"durability":"async"` in those modes and `"durability":"local"` in
single-instance/shared-PV mode.

### Upload state machine

The server and UI share these externally meaningful states:

```text
idle -> validating-client -> receiving -> validating-server -> planning
     -> conflict-retained -> confirming -> committing -> committed
     -> cancelled | failed
```

- `receiving` may write unreferenced blobs but never artifact rows.
- `validating-server` reads only bounded package metadata from staged blobs.
- `planning` computes immutable assets and mutable index mutations.
- `conflict-retained` is Maven-only, has no visible artifact writes, leases
  staged blobs for 30 minutes, and returns all conflicting immutable paths plus
  a confirmation URL.
- `committing` is one SQLite transaction and is not cancellable after the first
  SQL mutation; request cancellation is checked immediately before it starts.
- `committed` is terminal even if the client disconnects before reading the
  response; the idempotency key retrieves the result.
- `cancelled` and `failed` leave only zero-reference staged blobs.

### Complete response types

```ts
type ArtifactUploadResult = {
  upload_id: string
  repository: string
  format: "maven" | "npm" | "cargo" | "go" | "pypi"
  coordinate: string
  created: UploadedArtifact[]
  replaced: UploadedArtifact[]
  derived: UploadedArtifact[]
  scan_status: "queued" | "deferred" | "disabled"
  durability: "local" | "async"
  warnings: UploadWarning[]
}

type UploadedArtifact = {
  path: string
  role: "primary" | "metadata" | "checksum" | "index"
  size: number
  sha256: string
}

type Problem = {
  type: string
  title: string
  status: number
  code: string
  detail: string
  upload_id?: string
  field_errors?: Record<string, string[]>
  conflicts?: string[]
  removes?: string[]
  conflict_action?: "replace" | "reject"
  confirm_url?: string
  expires_at?: string
  retryable?: boolean
}
```

`created` and `replaced` contain all publication-owned immutable paths, whether
uploaded or generated (for example a generated POM and its checksums). `derived`
contains shared mutable aggregate metadata and its checksums; updating a derived
path is never presented as an overwrite conflict.

## Backend design

### Ownership boundary

The management API currently owns repository metadata but not the blob engine.
Do not duplicate storage logic in `src/api`. Add an uploader interface to
the API handler and inject the existing `repo.Manager` from `src/bin/forklift.rs`:

The API delegates upload receipt, status, confirmation, cancellation, publication deletion, and Cargo yank changes to the repository manager. Outcomes and problems are returned as typed values.

The API handler resolves `{id}`, delegates, and is the only layer that serializes
JSON/problem responses. `repo.Manager` receives the request because it already
owns auth and source-IP semantics, but returns typed outcomes. It also owns the
engine, blob store, scan queues, license queues, audit recorder, and metrics.
This is the final v1 boundary; blob internals are not exported.

The delegated path must call shared access checks equivalent to native
repository resolution:

1. repository exists and is readable;
2. repository is hosted;
3. repository is not disabled;
4. source IP passes the repository ACL;
5. principal has `write` (and `delete` for Maven overwrite or deletion);
6. format has a registered UI uploader.

This order prevents the UI endpoint from becoming an ACL bypass.

### Format registry

Use a small explicit registry rather than Nexus's server-defined dynamic form
schema. Forklift has five built-in formats and a typed React frontend, so a
shared registry is easier to test and evolve:

Each format validates its manifest, stages package bytes, and derives a publication plan according to its mutation policy.

`MutationPolicy` is a closed enum-backed value, not repository configuration:
Maven is `replace/delete`, PyPI is `extend/delete-with-filename-tombstones`, npm
is `reject/unpublish-with-coordinate-tombstone`, Cargo is `reject/yank`, and Go
is `reject/retain`. The manager consults it before accepting `overwrite`, before
creating a confirmation plan, and when exposing publication actions.

`Manager` registers Maven, PyPI, npm, Cargo, and Go handlers. Each handler owns
archive inspection, coordinate derivation, canonical path creation, and derived
metadata generation. Common code owns multipart limits, staging, collision
checks, atomic commit, metrics, audit, and scan enqueueing.

The frontend keeps matching TypeScript form definitions. Add a contract test
that every format advertised by `capabilities.upload` has both a backend handler
and a frontend form. A future plugin format can introduce a server-provided
schema, but that complexity is unnecessary now.

### Parser and validation dependencies

The Rust implementation uses `toml` for Cargo manifests, `semver` with
format-specific validation for npm and Cargo versions, `quick-xml` for Maven
metadata, `flate2` and `tar` for compressed package archives, and `zip` for
module archives and wheels. Archive inspection is bounded and never extracts
untrusted paths into the filesystem. XML validation rejects document types.

Go module path, version, and zip rules are implemented in the repository's Go
protocol modules. Python metadata and version normalization are implemented by
the PyPI helpers and validated with ecosystem fixtures. Direct crate requirements
live in `Cargo.toml`, and resolved versions are recorded in `Cargo.lock`.
The server does not invoke package managers or archive command-line tools.

### Streaming and limits

Never call `ParseMultipartForm`; the current container cannot rely on writable
`/tmp`, and buffering package bodies is unsafe. Consume `MultipartReader`
sequentially and stream every file through `io.LimitReader` into the blob store.

Initial limits:

- 16 file parts maximum.
- Maven, npm, Cargo, and PyPI: 256 MiB per file; Maven and multi-distribution
  PyPI requests: 512 MiB total file bytes.
- Go: 500 MiB compressed zip and 500 MiB total uncompressed content, matching
  the Go module zip specification; allow bounded multipart overhead above that
  value at the HTTP server and reverse proxy.
- 64 KiB manifest and 1 MiB total non-file fields.
- Bounded archive metadata reads: only required entries, with limits on entry
  count, expanded bytes, path length, and compression ratio.

Make byte limits configuration values before increasing them. Reverse-proxy and
Ingress body limits must be documented to be at least the Forklift request
limit, or users will see a proxy-generated 413 before Forklift can return a
useful error.

### Atomic commit

A package upload often creates several logical artifacts. Current
`PutArtifact` calls commit one path at a time, which would expose partial Maven,
npm, Cargo, or Go packages after a mid-request failure.

Add an engine operation that:

1. Holds `gcMu.RLock` while uploaded and generated bytes are staged in the
   content-addressed blob store.
2. Validates every package and computes the complete target path set.
3. Checks all collisions after validation and immediately before commit.
4. Inserts/replaces all artifact rows and blob reference-count changes in one
   SQLite transaction through the new `meta.ApplyArtifactBatch` method.
5. Clears negative-cache entries, updates ingress metrics, records audit events,
   and queues one scan/license resolution per unique coordinate only after the
   transaction commits.

On validation failure, cancellation, conflict, or transaction failure, staged
bytes remain zero-reference blobs and are handed to the existing sweeper using
the `abandonBlob` pattern. They are never visible as repository artifacts.

For S3, blob writes cannot participate in the SQLite transaction. The ordering
above intentionally allows orphaned content-addressed objects, which are safe
and reclaimable, while preventing artifact rows from referencing missing bytes.

Concurrent uploads of the same target are serialized by the metadata
transaction and collision condition. Collision checking must occur inside that
transaction; a check-before-transaction alone has a time-of-check/time-of-use
race.

### Publication identity and lifecycle schema

Artifact paths alone cannot express that a Maven release has a POM, binary,
sources, checksums, and shared metadata, or that an npm version owns a tarball
but shares a packument with other versions. Add a lightweight component model:

```sql
CREATE TABLE artifact_publications (
  id           TEXT PRIMARY KEY,
  repo_id      INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  format       TEXT NOT NULL,
  package_name TEXT NOT NULL,
  version      TEXT NOT NULL,
  coordinate   TEXT NOT NULL,
  upload_id    TEXT NOT NULL,
  created_by   TEXT NOT NULL,
  created_by_source TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL,
  UNIQUE(repo_id, format, package_name, version)
);

ALTER TABLE artifacts ADD COLUMN publication_id TEXT
  REFERENCES artifact_publications(id) ON DELETE SET NULL;
ALTER TABLE artifacts ADD COLUMN artifact_role TEXT NOT NULL DEFAULT 'primary';
CREATE INDEX idx_artifacts_publication ON artifacts(publication_id);

CREATE TABLE artifact_publication_tombstones (
  repo_id       INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  format        TEXT NOT NULL,
  package_name  TEXT NOT NULL,
  version       TEXT NOT NULL,
  asset_key     TEXT NOT NULL,
  deleted_at    TEXT NOT NULL,
  deleted_by    TEXT NOT NULL,
  PRIMARY KEY(repo_id, format, package_name, version, asset_key)
);

CREATE TABLE group_metadata_cache (
  group_repo_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
  path          TEXT NOT NULL,
  representation TEXT NOT NULL,
  blob_sha256   TEXT NOT NULL REFERENCES blobs(sha256),
  size          INTEGER NOT NULL,
  sources_json  TEXT NOT NULL,
  config_revision TEXT NOT NULL,
  expires_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  PRIMARY KEY(group_repo_id, path, representation)
);
```

Tombstones enforce ecosystem reuse rules after visible deletion. npm writes
`asset_key="*"` and permanently blocks the package/version coordinate. PyPI
writes one tombstone per deleted distribution filename and blocks only reuse of
those URLs; a later upload may add a genuinely new filename to the same release.
Maven does not write a tombstone. Cargo yanking and Go's lack of deletion keep
their publication rows, so neither needs a tombstone.

`group_metadata_cache` participates in blob reference counting. Replacing or
invalidating a row decrements the old digest in the same transaction; the row
is an internal cache record, not an artifact. `sources_json` is a bounded,
deterministically ordered array of `{repo_id,path,validator}` and is never
accepted from a client.

Roles are `primary`, `metadata`, `checksum`, and `index`. Caller-owned immutable
assets use the publication ID. Shared mutable paths such as npm packuments,
Cargo sparse-index files, Maven A-level metadata, and Go `list`/`@latest` have a
null publication ID and role `index`; they are rebuilt from remaining package
state and are not owned by one version.

`artifacts.metadata_json` uses one versioned envelope for managed artifacts:

```json
{
  "schema_version": 1,
  "managed_by": "ui_upload",
  "upload_id": "01J...",
  "format": "pypi",
  "package": "sample-project",
  "version_identity": "1.0",
  "role": "primary",
  "source_filename": "sample_project-1.0-py3-none-any.whl",
  "format_metadata": {}
}
```

The first seven fields are required for every publication-owned row;
`source_filename` is required for uploaded bytes and absent for generated
bytes. `format_metadata` is an object with a schema owned by the format
handler: Maven records extension/classifier/generated-POM status; PyPI records
display version, distribution kind, metadata version, `requires_python`, and
wheel tags; npm records selected tag; Cargo records checksum and index schema
version; Go records module time and whether `.mod` was supplied, extracted, or
generated. Shared mutable rows use `managed_by="ui_upload_aggregate"`, omit
`upload_id` and `source_filename`, and record `package` plus
`aggregate_schema_version`. Readers must reject an unknown `schema_version`
when mutation depends on it, returning `derived_metadata_not_managed`; display
paths may treat unknown fields as opaque. JSON is serialized deterministically.

Native artifacts created before the migration have no publication ID and
remain fully readable/deletable. Native npm and PyPI publish paths are refactored
to create publications. Maven/Cargo/Go raw PUT remains path-oriented and writes
null publication IDs in v1; it never guesses component membership. Before any
raw PUT, however, it looks up the target path and returns 409 `managed_artifact`
when `publication_id` is non-null or `managed_by` identifies a UI aggregate.
Thus legacy raw paths keep their prior behavior without allowing raw PUT to
replace a managed Maven, Cargo, or Go file/index.

### Mutation classes and collision rules

Every format handler returns an `UploadPlan`:

An upload plan records its upload ID, publication, immutable assets, mutable aggregate metadata with expected digests, scan targets, and warnings.

- **Immutable** means a package/version-owned path. Path absence is required for
  every new publication. PyPI may attach new paths to its existing publication
  when their filenames have never existed or been tombstoned. With Maven
  `overwrite=true`, all conflicting immutable paths must belong to the same
  package/version publication; replacing an unrelated raw artifact is refused
  with `artifact_ownership_conflict` even for admins. Other formats reject
  `overwrite=true`.
- **Mutable** means server-derived aggregate metadata. It is always updated by
  compare-and-swap and never requires the caller's delete permission. The
  planner records the digest it read; the transaction fails with
  internal sentinel `derived_metadata_changed` if that digest changed. This
  sentinel triggers the bounded replan below and is never returned as a public
  problem code.
- **Generated immutable** checksums/POMs follow their owning primary asset's
  collision decision. A user cannot replace only a checksum.

Maven overwrite requires both `write` and `delete`, creates a new `upload_id`,
keeps the existing publication ID, updates `updated_at`, and replaces the
complete version-owned asset set. Omitted old classifiers are removed so the
publication exactly matches the reviewed request. The confirmation response
lists both paths to replace and paths to remove. PyPI extension preserves every
existing distribution and adds the new files to the same publication ID.

### Keyed planning lock and batch transaction

Use an in-process keyed mutex on `(repoID, format, packageName)` from the point
where existing aggregate metadata is opened through transaction completion.
Only the elected leader receives writes, so this removes ordinary same-package
races without globally serializing unrelated uploads. The SQLite transaction
still performs digest compare-and-swap, which protects against raw native PUTs
that do not take this lock.

`meta.ApplyArtifactBatch` is the only commit primitive:

The artifact batch records the expected upload state, publication, creates, replacements, removals, tombstones, metadata digest checks, group cache invalidations, and committed response JSON.

Execute the batch on the metadata writer connection in an immediate SQLite
transaction. Commit all changes together or roll them back on any error.

Inside one `BEGIN IMMEDIATE` transaction it must:

1. Re-read the upload request and require `ExpectedUploadState` (`receiving`
   for first-pass commit, `conflict` for confirmed replacement).
2. Re-check publication and immutable path expectations, collecting all
   conflicts before any mutation.
3. Re-check every mutable path's expected digest/absence.
4. Ensure all new blob rows exist with their staged sizes.
5. Insert/update the publication.
6. Remove superseded publication paths and decrement old blob refs grouped by
   digest; insert required npm/PyPI tombstones in the same transaction.
7. Insert/update new immutable and mutable artifacts, incrementing/decrementing refs
   exactly once when digests differ.
8. Assert no blob ref count is negative.
9. Delete any staged-blob leases for this upload now that artifact refs exist.
10. Delete affected `group_metadata_cache` rows and decrement their blob refs;
    the planner resolves reverse group membership and exact aggregate keys
    before the transaction. Repository group-config changes purge every cache
    row for that group through the settings transaction.
11. Persist result JSON, clear `plan_json`, and transition the idempotency row
    to `committed`.
12. Commit.

Conflict or CAS failure rolls the transaction back. A mutable CAS failure is
replanned under the keyed lock up to two times; a third failure returns 409
`concurrent_metadata_update` with `retryable=true`. Do not retry storage,
validation, authorization, or immutable collision errors.

### Deletion and metadata reconciliation

Lifecycle operations must follow the format policy while repairing aggregate
metadata. Add:

```http
DELETE /api/v1/repositories/{id}/publications/{publicationID}
```

It is valid only for Maven, PyPI, and npm and requires `delete`, CSRF for cookie
auth, current leader, and source-IP ACL. It takes the keyed package lock and
applies artifact removal, tombstone insertion where required, and index changes
in one `ApplyArtifactBatch`-style transaction. Maven rebuilds or removes its
A-level metadata. PyPI removes files from its synthesized listing and writes
filename tombstones. npm removes the version/time/tag entries, writes the
coordinate tombstone, and removes the packument when no versions remain.

Return 200 with `{publication_id, coordinate, deleted:[paths],
derived_updated:[paths], derived_deleted:[paths]}`. A repeated delete after the
publication is gone returns 404; the confirmation dialog prevents accidental
retries, and deletion does not reuse upload idempotency keys.

Cargo instead exposes `POST .../publications/{id}/yank` with
`{"yanked":true|false}`. It requires `write`, updates only the matching index
line's `yanked` field by CAS, and retains the crate and publication. Go exposes
neither lifecycle endpoint. Calling an unsupported operation returns 409
`lifecycle_not_supported` and no mutation.

The Artifacts UI groups rows by publication and renders **Delete version**,
**Unpublish version**, **Yank/Unyank**, or no destructive action from the DTO's
`actions`. Existing raw path deletion remains an administrator-only repair
surface. If a raw delete targets a publication-owned or shared-derived path,
return 409 `managed_artifact` with the valid lifecycle URL when one exists;
never create stale indexes through the legacy endpoint.

### Format rules

#### Maven

- Allow multiple assets, but require unique `(extension, classifier)` pairs.
- If a POM is supplied, parse it with a bounded XML decoder and use its GAV;
  reject external entities and coordinate mismatches.
- Without a POM, require GAV and `generate_pom=true`; generate a minimal POM.
- Rename assets to canonical Maven filenames rather than persisting arbitrary
  local filenames.
- Generate SHA-1 and SHA-256 checksum sidecars for the POM and every asset. MD5
  is not generated; a client that explicitly requests it receives 404.
- Merge A-level `maven-metadata.xml` with existing releases, recompute
  `latest`, `release`, `versions`, and `lastUpdated`, and generate checksum
  sidecars. Store the POM, assets, checksums, and metadata atomically.
- For `packaging=maven-plugin`, require an unclassified JAR containing one
  bounded `META-INF/maven/plugin.xml`, cross-check its GAV, extract
  `goalPrefix`, and atomically reconcile G-level plugin metadata. This makes
  prefix invocation such as `mvn prefix:goal` work, not only exact GAV use.
- UI upload supports release versions. `*-SNAPSHOT` is rejected with a clear
  message because correct publication requires timestamp/build-number V-level
  metadata and timestamped filenames. Native Maven deployment remains the path
  for snapshots until that separate contract is implemented. This is an
  explicit version-mode limitation, not an unsupported repository format.

#### PyPI

- Reuse the existing normalized project naming and hosted simple-index model.
- Support multiple wheels and one source distribution for the same project
  release in one request. Current standard sdists are `.tar.gz`; legacy `.zip`
  sdists require the repository compatibility option.
- Inspect bounded metadata and reject a filename/name/version disagreement.
- Preserve the original distribution filename after `path.Base` validation;
  reject separators, control characters, and traversal.
- For a wheel, locate exactly one `{distribution}-{version}.dist-info/METADATA`,
  `WHEEL`, and `RECORD`; parse required metadata fields and validate the
  dist-info directory plus expanded `Tag` headers against the wheel filename.
  For an sdist, distinguish the current standardized format from legacy input
  and validate its name/version against the filename.
- Commit all distribution files together. The existing hosted simple-index
  response is synthesized from stored artifacts, so no separately persisted
  project index needs to be updated; add SHA-256 fragments to generated links.

#### npm

- Require one `.tgz` whose archive contains a single bounded
  `package/package.json` metadata entry.
- Validate package names using npm's lowercase scoped/unscoped rules. Require a
  canonical strict SemVer 2.0 version; reject coercions such as a leading `v`.
  Reject a dist-tag if npm's semver-range grammar could interpret it as a
  version/range, or if it contains URL/path/control characters.
- Reject packages with `private: true`; the UI must respect the package author's
  explicit non-publication intent.
- Build the tarball path from the canonical name/version.
- Merge the new version into an existing hosted packument instead of replacing
  other versions. Preserve unrelated package metadata and dist-tags; set the
  selected tag to this version; recompute `dist.tarball`, SHA-1 `shasum`,
  SHA-512 SRI `integrity`, and the version's publish time.
- Commit tarball and packument together. Refactor native `npmPublish` to call the
  same package-level service so UI and CLI behavior cannot drift.
- A conflict is keyed by package plus canonical version, not only tarball
  filename. Existing versions and npm tombstones are permanent conflicts; UI
  upload never overwrites or reuses them.

#### Cargo

- Inspect `.crate` as a bounded gzip/tar archive. Require one top-level crate
  directory and parse its normalized `Cargo.toml`; use `Cargo.toml.orig` only as
  supplementary display metadata.
- Validate Cargo package name and semver. Derive the lowercase sparse-index path
  exactly: `1/{name}` for one character, `2/{name}` for two, `3/{first}/{name}`
  for three, and `{first-two}/{second-two}/{name}` for four or more.
- Generate the index JSON line with `name`, `vers`, normalized dependencies,
  features, SHA-256 `cksum`, `yanked`, optional `links`, `rust_version`, and
  schema version where required. Publish-API and index dependency field names
  differ; the registry writer must perform the documented conversion rather
  than serialize `Cargo.toml` directly.
- Merge by version without reordering or dropping older newline-delimited index
  entries. Append a new version; reject a duplicate SemVer identity, including
  versions that differ only by build metadata. Existing entries are never
  replaced except for a dedicated yank/unyank update to `yanked`.
- Store the crate download artifact and index update atomically.
- `config.json` omits `api` until Forklift implements Cargo's registry web API;
  advertising an unusable publish endpoint is invalid. Set
  `auth-required:true` whenever reads require credentials so Cargo forwards its
  configured credential token to sparse-index and crate download requests.
- Refactor any future native Cargo publish API to use the same service.

#### Go

- Validate the user-supplied module path and version with the Rust module-validation helpers
  (`module.CheckPath`, canonical version and major-version suffix), and derive
  the GOPROXY case-escaped storage path with `module.EscapePath` and
  `module.EscapeVersion` rather than a local approximation.
- Validate the module zip root and path rules with the Rust module-ZIP validator. Every
  file must share the exact `$module@$version/` prefix; reject case-folding
  collisions, nested `go.mod` files, invalid Windows names, and size violations.
- Extract a bounded `go.mod`; reject a module directive or version that differs
  from the claimed module and zip root. If the zip has no top-level `go.mod`,
  accept the optional separate file or generate a minimal synthetic one.
- Generate `.info` as `{"Version":"...","Time":"RFC3339"}`. Merge only
  canonical tagged release/pre-release versions into newline-delimited `list`;
  the GOPROXY protocol explicitly excludes pseudo-versions from this endpoint.
  Compute `@latest` from release, pre-release, then pseudo-version precedence.
- Store `.zip`, `.mod`, `.info`, and applicable `list`/`@latest` changes
  atomically. A published version can never be replaced or deleted: successful
  GOPROXY `.mod` and `.zip` responses must remain byte-identical.

### Per-format publication plans

The following plans are normative. Handlers may be organized differently in
code, but paths, ownership, merge behavior, and deletion results must match.

Stored content types are server-owned: Maven uses the existing
`mavenContentType`; PyPI wheels are `application/zip` and sdists
`application/gzip`; npm tarballs are `application/octet-stream` and packuments
`application/json`; Cargo crates are `application/gzip` and sparse indexes
`text/plain; charset=utf-8`; Go uses the existing `go_content_type`. Generated XML
is `application/xml`, JSON is `application/json`, and checksum sidecars are
`text/plain; charset=utf-8`.

For a new publication, capture one UTC `publicationTime` after validation and
use it for publication `created_at`, primary artifact `published_at`, npm time,
and tagged Go info unless the format supplies or derives an authoritative time.
All artifact
`cached_at`, `last_accessed_at`, and `updated_at` values use the commit time and
`cached_by` uses the authenticated username. Maven replacement preserves the
publication's original `created_at`/publication time and updates only
`updated_at`; newly added Maven classifiers inherit that time. A PyPI extension
preserves publication `created_at` but each newly added distribution records its
actual current `published_at`/Simple API `upload-time`. Shared derived artifacts
have empty version, null `published_at`, and the current uploader in `cached_by`
for auditability.

#### Maven plan

For GAV `G:A:V`, let `D = strings.ReplaceAll(G, ".", "/") + "/" + A`,
`B = D + "/" + V + "/" + A + "-" + V`.

| Path | Role | Ownership |
| --- | --- | --- |
| `B.pom` | metadata | publication immutable |
| `B[-classifier].extension` | primary | publication immutable |
| each `.sha1` / `.sha256` | checksum | publication immutable for asset/POM; mutable for A-level metadata |
| `D/maven-metadata.xml` | index | shared mutable |
| `group/path/maven-metadata.xml` for `maven-plugin` | index | shared mutable |

Algorithm:

1. Treat Maven's Java-package-style group ID as a recommendation, not a wire
   restriction. Require dot-separated non-empty `[A-Za-z0-9_-]+` group segments
   and reject `.`/`..`; validate artifact/version/classifier/extension as
   non-empty URL/path-safe segments with no slash, backslash, control character,
   leading dot, or `..`. Percent-encode generated download URLs without changing
   the decoded repository key.
2. Identify at most one unclassified `pom` asset. Parse its effective GAV using
   direct project values with explicit parent fallback. If present, it must
   match the manifest. Reject `${...}` in effective GAV with
   `pom_requires_flattening`; CI-friendly source POMs must be flattened before
   repository publication or consumers may not resolve them. If absent,
   generate a minimal Maven 4.0.0 POM containing modelVersion, GAV, and packaging.
3. Canonicalize every asset name from GAV, classifier, and extension. Reject
   duplicate canonical paths, including duplicates created by case folding on
   case-insensitive clients.
4. Stage every primary/POM plus SHA-1 and SHA-256 text sidecars containing
   lowercase hex followed by `\n`.
5. Parse existing A-level metadata with the same bounded XML rules. Accept only
   the standard `groupId`, `artifactId`, and `versioning/{latest,release,
   versions/version,lastUpdated}` elements plus namespace/schema attributes.
   An unknown element returns 409 `derived_metadata_not_managed` rather than
   silently discarding third-party data. Regenerate the accepted model from
   publications and recognized legacy versions. A legacy version is recognized
   only when an existing path under `D/<candidate>/` has basename
   `A-<candidate>.pom` or `A-<candidate>.<extension>` and `<candidate>` passes
   the same release-version validation; checksum/signature-only paths do not
   create a version.
6. Preserve the existing `versions` order and append `V` once. Maven defines
   `latest` as the last version added and `release` as the last release added,
   not the numerically greatest version. Set both to `V` for this release and
   set UTC `lastUpdated=yyyyMMddHHmmss`. On deletion of the current value, use
   the surviving managed publication with greatest `created_at`; when only
   legacy entries remain, use the last surviving entry in the preserved list.
   Generate metadata checksums.
7. When packaging is `maven-plugin`, require an unclassified JAR with exactly
   one regular `META-INF/maven/plugin.xml`. Bounded-parse `groupId`,
   `artifactId`, `version`, `goalPrefix`, and `name`; cross-check GAV, require a
   non-empty safe prefix, use artifact ID when name is absent, and merge
   `{name,prefix,artifactId}` in G-level
   metadata at the group path. Reject malformed or unknown G-level metadata
   with `derived_metadata_not_managed`; reject a prefix already owned by a
   different artifact as `plugin_prefix_conflict`. The planner acquires package
   and group locks in lexical order. Deletion removes the plugin entry only when no
   remaining publication of that GA supplies it.
8. Batch commit. On deletion, remove `V`; delete A-level metadata when no
   version remains, otherwise recompute the versioning fields and checksums.

Maven UI upload intentionally rejects signatures presented as a classifierless
`.asc` asset because their relationship is ambiguous. A signature is accepted
only as an asset whose extension is `<target-extension>.asc` and classifier
matches the target, and the UI displays that mapping before commit.

#### PyPI plan

Let `N` be the PEP 503 normalized project name, `OV` the exact core metadata
version used for display, and `V` its canonical PEP 440 identity used for
publication uniqueness and equality.

| Path | Role | Ownership |
| --- | --- | --- |
| `packages/N/original-filename` | primary | publication immutable |
| simple project/index responses | generated at request time | not persisted |

Algorithm:

1. For a wheel, parse its standardized filename into distribution, version,
   optional build tag, Python tag, ABI tag, and platform tag. Require one
   correctly escaped `.dist-info` directory containing exactly one `METADATA`,
   `WHEEL`, and `RECORD`. Require metadata identity to match the filename under
   PEP 440/name normalization. Require `Wheel-Version`, `Root-Is-Purelib`, and
   at least one `Tag`; reject a greater unsupported major and require the set of
   expanded filename tags to equal the `Tag` header set. Validate `RECORD` is a
   regular file but do not recalculate its hashes because upload does not alter
   wheel contents.
2. For a `.tar.gz` sdist, require one top-level `{name}-{version}` directory and
   `PKG-INFO`. If `pyproject.toml` is present, treat it as the current standard:
   require `Metadata-Version >= 2.2`, validate license-file paths for metadata
   2.4+, and apply the standardized filename rules. Without `pyproject.toml`,
   accept it as a legacy tar sdist but still require valid core identity and the
   archive safety rules. Legacy `.zip` follows the legacy rules only when the
   repository compatibility option is enabled.
3. All assets in a request must share `(N,V)` and have unique filenames. Store
   Before normalization, every metadata `Name` must satisfy PyPA's ASCII name
   grammar and start/end with an alphanumeric character; normalization must not
   turn invalid input into a valid project. Store OV in artifact `version` and
   V in publication identity. Record
   `requires_python`, metadata version, distribution kind, and wheel tags in
   `Artifact.MetadataJSON` for simple-index rendering and UI detail.
4. Commit the files as one publication. If `(N,V)` already exists, append only
   new, non-tombstoned filenames to that publication; any existing or
   tombstoned filename returns `distribution_filename_reused`. The hosted
   `/simple/N/` response sorts filenames lexicographically, emits
   percent-encoded links with `#sha256=<digest>`, and adds
   `data-requires-python` when known. PEP 691 JSON advertises API 1.1 and includes
   required `filename`, `url`, `hashes`, and `size`, plus `versions`,
   `requires-python`, and `upload-time` when known. `upload-time` is UTC
   `yyyy-mm-ddThh:mm:ss[.ffffff]Z` with at most six fractional digits. HTML advertises repository
   version 1.0. Both are selected by `Accept` with the exact standardized media
   types and send `Vary: Accept`.
5. Deleting a publication removes all its distributions. The synthesized index
   immediately reflects remaining files; if none remain, `/simple/N/` returns
   404 and the root simple index omits `N`. `GET /simple/` is synthesized for
   hosted repositories in both PEP 691 JSON (`meta`, `projects`) and HTML (one
   normalized project link each); it is not treated as an empty project name.

`overwrite=true` is invalid for PyPI. Extending a release never removes existing
files, and deletion writes filename tombstones in the same transaction so a
cached distribution URL can never be rebound to different bytes.

#### npm plan

Let `P` be the canonical package name, `U` its unscoped basename, and `V` the
validated version.

| Path | Role | Ownership |
| --- | --- | --- |
| `P/-/U-V.tgz` | primary | publication immutable |
| `P` | index | shared mutable packument |

Algorithm:

1. Stream the gzip/tar and capture exactly one regular-file
   `package/package.json`, maximum 1 MiB. Reject duplicate entries, links for
   package.json, `private:true`, missing name/version, and a manifest name whose
   normalized scoped identity differs from `P`. Require at most 214 ASCII
   characters, lowercase URL-safe package segments, no whitespace/control/path
   traversal, and either `name` or exactly `@scope/name` with non-empty segments
   that do not begin with `.` or `_`. Require canonical strict SemVer 2.0 `V`;
   validation is deliberately a
   safe subset of npm's loose CLI input and never coerces a stored version.
   Validate `selectedTag` by the npm dist-tag grammar and reject any value that
   npm's semver-range parser would accept.
2. Compute tarball SHA-1 and SHA-512 while staging. Build the version manifest
   from package.json after removing `private`, `publishConfig`, `_id`, `_from`,
   `_resolved`, and any existing `dist`; add canonical `_id=P@V` and server-owned
   `dist.tarball`, `dist.shasum`, and `dist.integrity`.
3. Read the existing packument or initialize `{name:P, versions:{},
   dist-tags:{}, time:{created:now,modified:now}}`. Require its `name` matches.
   Preserve unknown top-level fields, existing versions, and tags. Invalid JSON
   or wrong types for `versions`, `dist-tags`, or `time` return
   `derived_metadata_not_managed`; never replace a malformed native packument.
4. Require `versions[V]` absent and no npm coordinate tombstone. Insert
   `versions[V]`, set `dist-tags[selectedTag]=V`, set `time[V]`, update
   `time.modified`, and compute top-level `_id=P`. Do not store `_attachments`.
5. Serialize deterministically with stable top-level/version/tag key ordering so
   identical logical updates deduplicate. Commit tarball and packument.
6. On unpublish, atomically write the permanent `*` coordinate tombstone, remove
   `versions[V]` and `time[V]`, and delete non-`latest` tags
   pointing to V. If `latest` pointed to V, move it to the highest remaining
   stable semver, otherwise highest prerelease; delete it if none remain. Remove
   the packument when no versions remain.

The existing request-time packument rewrite remains responsible for generating
the externally visible tarball URL, so the stored URL is a host-independent
canonical relative marker and cannot be poisoned by request headers.

#### Cargo plan

Let `C` be the package name exactly as declared, `L = strings.ToLower(C)`, `V`
the Cargo semver, and `VI` its uniqueness identity with build metadata removed.

| Path | Role | Ownership |
| --- | --- | --- |
| `api/v1/crates/C/V/download` | primary | publication immutable |
| Cargo sparse path for `L` | index | shared mutable |

Algorithm:

1. Require a regular gzip/tar `.crate` with one top-level `C-V/` directory.
   Capture bounded `Cargo.toml`, optional `Cargo.toml.orig`, and reject duplicate
   normalized paths or identity mismatches. Require 1–64 ASCII characters, an
   alphabetic first character, and only alphanumeric, `-`, or `_`; reject
   Windows reserved names. Enforce repository-wide case-insensitive and
   hyphen/underscore collision identity so visually/interoperably equivalent
   crate names cannot coexist.
2. Parse the normalized manifest. Resolve dependency tables including target
   tables and renamed dependencies into registry-index fields: alias in `name`,
   original package in `package`, requirement in `req`, feature list, optional,
   default_features, target, kind, and registry. For this non-crates.io
   registry, translate an unspecified registry dependency to the crates.io
   index URL and translate a dependency explicitly targeting Forklift's current
   index to `null`, as required by the index format. Path dependencies without
   a version requirement are invalid in a published crate. Comparison with the
   current registry uses only `FORKLIFT_EXTERNAL_URL`, never request headers; an
   explicit same-registry URL without a configured canonical external URL
   returns `canonical_external_url_required`.
3. Compute SHA-256 of the exact `.crate`. Create the index object with `name`,
   `vers`, `deps`, `cksum`, `features`, optional `features2`/`v`, `yanked=false`,
   optional `links`, `rust_version`, and `pubtime` formatted as whole-second UTC.
   Include `features2` only with `v:2`; otherwise omit both.
4. Read newline-delimited existing index entries. Reject malformed lines rather
   than dropping them. Require `VI` unique, preserve every existing byte-valid
   line in its original order, and append V. Never reorder old lines merely for
   deterministic output.
5. Commit crate and index. The lifecycle operation toggles only `yanked` on V's
   line and retains the crate bytes; physical deletion is not exposed.

No `cargo search`, owners, yank, or native publish API is implied by UI upload.
The generated `config.json` advertises Forklift's download endpoint and the
correct `auth-required` value. `api` remains omitted until native publish exists.

#### Go plan

Let `M` and `V` be validated unescaped module path/version, and `EM`/`EV` their
GOPROXY case-escaped forms. Let `D = EM + "/@v/"`.

| Path | Role | Ownership |
| --- | --- | --- |
| `D+EV.zip` | primary | publication immutable |
| `D+EV.mod` | metadata | publication immutable |
| `D+EV.info` | metadata | publication immutable |
| `D+"list"` | index | shared mutable |
| `EM+"/@latest"` | index | shared mutable |

Algorithm:

1. Validate M/V and zip with `x/mod`. The root inside the zip uses unescaped
   `M@V/`; the HTTP/storage path uses EM/EV. A supplied `.mod` and a zip
   top-level `go.mod` must be byte-identical after normalizing one trailing
   newline and must declare M. Otherwise choose zip mod, supplied mod, then a
   generated `module M\n` in that order.
2. For a pseudo-version, derive `.info.Time` from its encoded UTC timestamp with
   `module.PseudoVersionTime`; an administrator-supplied time must equal it. For
   a tagged release/pre-release, use an administrator-supplied source time or
   the UTC server publication time. Serialize `.info` deterministically with
   Version and RFC3339Nano Time.
3. Read existing list lines. A non-empty invalid/non-canonical or pseudo-version
   line returns `derived_metadata_not_managed`. Keep canonical tagged versions,
   remove exact duplicates, add V only when it is not a pseudo-version, and
   sort with `x/mod/semver.Compare`.
4. Read existing publications plus legacy versions that have a valid matching
   `.info`, `.mod`, and `.zip` trio under D. Parse their info timestamps and
   compute `@latest`: highest release tag, otherwise highest prerelease tag,
   otherwise the newest pseudo-version timestamp. Serialize the chosen
   version's exact info object. A malformed legacy trio returns
   `derived_metadata_not_managed` instead of being dropped.
5. Reject any existing publication/path conflict with
   `immutable_version_exists`. Commit the three immutable files and applicable
   list/latest mutations atomically. No UI deletion or replacement is defined.

Forklift does not implement the public checksum database. Documentation and the
success view tell users to configure `GONOSUMDB`/`GOPRIVATE` for private module
paths; the proxy still serves immutable module bytes correctly.

### Audit and observability

- Add `request_id TEXT NOT NULL DEFAULT ''` and
  `detail_json TEXT NOT NULL DEFAULT ''` to `audit_logs`. Record one `upload`
  event for every committed caller-owned logical path, attributed to the session
  user, with method, status, client IP, user agent, and `request_id=upload_id`.
  `detail_json` contains format, coordinate, role, created/replaced, and digest
  prefix. Record one `upload.metadata` event summarizing derived index changes
  instead of one noisy row per generated checksum.
- Emit no success audit event for a rolled-back upload. Record rejected attempts
  as `upload.reject` only after authentication and repository resolution, with
  no file contents or raw parser errors. Cancellation is `upload.cancel`.
- Continue `forklift_bytes_transferred_total{direction="ingress",format=...}`
  for committed bytes only.
- Add `forklift_ui_uploads_total{format,result}` with
  `success|validation|conflict|denied|error` and
  `forklift_ui_upload_duration_seconds{format}`.
- Add `forklift_group_metadata_merges_total{format,result}`,
  `forklift_group_metadata_merge_duration_seconds{format}`, and
  `forklift_group_metadata_cache_total{format,result="hit|miss|invalidated"}`;
  group/repository/path remain logs, not metric labels.
- Never log manifest secrets or file contents. Coordinates, resulting paths,
  sizes, digest prefixes, uploader, and upload ID are sufficient.

After commit, de-duplicate `ScanTargets` by ecosystem/package/version and
enqueue each once for vulnerability and license processing. Upload success never
waits for external
[OSV](https://github.com/google/osv.dev)/[deps.dev](https://github.com/google/deps.dev)
calls. `queued` means both enabled queues accepted the target, `deferred` means
at least one queue was full and periodic backfill will recover it, and
`disabled` means neither integration is configured. The result warning names a
deferred subsystem without treating publication as failed.

### Security

- Treat file extension, MIME type, original filename, manifest, and archive
  contents as untrusted and cross-validate them.
- Reject absolute paths, `..`, NUL/control characters, duplicate normalized
  paths, symlinks where the format forbids them, and decompression bombs.
- Use bounded XML/JSON/TOML/archive parsing and request-context cancellation.
- Enforce same-origin checks for cookie-authenticated multipart mutations in
  line with the application's CSRF posture. Basic/token-authenticated native
  clients remain on their existing protocol endpoints.
- Do not execute package hooks, evaluate build scripts, or extract full
  archives to disk.
- Do not let UI upload bypass repository disabled state, ACL, RBAC, or quotas.

### CSRF and authentication

The current session cookie is `HttpOnly` and `SameSite=Lax`; keep those flags
but add a signed CSRF claim to the session and expose its opaque token from
`GET /api/v1/me` as `csrf_token`. The React API client sends it in
`X-CSRF-Token` for upload, confirm, and cancel requests. Validation uses
constant-time comparison and binds the token to the signed session, so it does
not require server-side CSRF storage.

Requests authenticated by `Authorization: Bearer` or Basic/PAT do not require a
CSRF token. A request that presents both a session cookie and Authorization is
treated as Authorization-authenticated and the audit principal must match that
credential. Browser UI uses only the cookie path.

Cargo is the one native-auth exception: its credential provider sends the
registry token as the entire `Authorization` value, without `Bearer`. On paths
under `/cargo/`, and only there, auth resolution accepts a scheme-less value
only when `auth.IsPAT(value)` succeeds; arbitrary raw tokens and this syntax on
other routes remain rejected. A Cargo 401 uses
`WWW-Authenticate: Cargo login_url="<external-settings-token-url>"` instead of a
Basic challenge. Operator/client documentation configures the Forklift PAT via
`cargo login --registry <name>` or `CARGO_REGISTRIES_<NAME>_TOKEN`. This behavior
and `config.json`'s `auth-required:true` are covered by a real Cargo E2E test.

Also reject cookie-authenticated unsafe requests when `Sec-Fetch-Site` is
`cross-site` or when a present `Origin` does not match `FORKLIFT_EXTERNAL_URL`
(or the validated forwarded request origin when external URL is unset). The
token is the primary control; origin checks are defense in depth.

### HA and leadership

Application traffic is intended to route only to the elected leader, but direct
pod access and label-transition races are possible. Add a write gate injected
from `src/bin/forklift.rs`:

The write gate captures a leadership term and cancellation signal at upload start, then validates that term before committing.

Upload start requires `ok`. The request context is cancelled when
`leadershipDone` closes. Before entering `ApplyArtifactBatch`, check
`StillLeader(term)`; failure returns 503 `leadership_changed` and leaves staged
blobs unreferenced. Once the SQLite transaction begins it runs to completion,
but a response is returned only if the term is still current. The idempotency
record resolves the rare case where leadership changes just after commit.

This design uses the same single-writer model as existing native uploads:

- shared-PV HA routes writes to the ready leader;
- replication and S3 HA keep all pods ready but the Service selects the leader
  label;
- S3 metadata snapshots and PV replication remain asynchronous, so the existing
  configured RPO applies and is surfaced in the result's `durability` field.

Conflict confirmation is not portable across a lost metadata term. If failover
loses its plan row, confirmation returns 404 and the UI asks for a fresh upload.
If the plan replicated, the new leader verifies every staged blob exists before
replanning; a missing blob returns 409 `staged_blob_missing` and releases the
plan.

### Resource control and timeouts

Acquire the per-principal and global upload semaphores after authorization but
before inserting `receiving`. Wait at most five seconds, then return 429. A
principal is identified by stable authenticated username plus source, not IP.
Conflict plans release concurrency slots; confirmation acquires them again.

Wrap receive, validation, and planning in `UploadConfig.MaxDuration`. Do not set
a global `http.Server.WriteTimeout`, which would also break long package
downloads. The HTTP server keeps `ReadHeaderTimeout=10s`; body duration is owned
by this handler. [Kubernetes](https://github.com/kubernetes/kubernetes)
termination cancels requests after the existing graceful-shutdown timeout,
safely leaving staged blobs.

Deployment requirements for defaults:

- Ingress/proxy maximum body: at least 525 MiB plus multipart overhead.
- Ingress proxy read/send timeout: at least 30 minutes.
- Leader ephemeral staging capacity in S3 mode: at least
  `MaxConcurrent * (GoMaxZipBytes + ArchiveMaxMetaBytes)` plus the normal SQLite
  and S3 blob temp footprint. With defaults, reserve at least 4 GiB.
- Filesystem mode writes staged blobs into the existing CAS temp directory and
  atomically renames them; no separate container `/tmp` dependency is added.

Reject a known oversized `Content-Length` before sending `100 Continue`; still
enforce all limits while streaming for chunked requests. Support
`Expect: 100-continue` so CLI/API callers can avoid transferring a body rejected
by auth, repository type, idempotency, or concurrency checks.

### Error catalogue

Every error code is stable API surface and maps to one UI recovery action:

| Code | Status | Retryable | UI action |
| --- | ---: | --- | --- |
| `upload_disabled` | 404 | no | Return to Artifacts |
| `repository_not_hosted` | 422 | no | Explain hosted target requirement |
| `repository_disabled` | 503 | yes | Return; retry after repository is online |
| `forbidden`, `ip_not_allowed` | 403 | no | Return; no replace control |
| `csrf_invalid` | 403 | after re-login | Refresh session or sign in |
| `upload_busy` | 429 | yes | Keep form and retry after countdown |
| `upload_in_progress` | 409 | yes | Poll/replay with same idempotency key |
| `idempotency_key_consumed` | 409 | with new key | Generate new attempt |
| `multipart_invalid`, `manifest_invalid` | 400 | after edit | Focus error summary/field |
| `asset_too_large`, `batch_too_large`, `too_many_assets` | 413 | after edit | Remove/replace files |
| `unsupported_asset` | 415 | after edit | Show accepted types |
| `metadata_invalid`, `coordinate_mismatch`, `archive_unsafe` | 422 | after edit | Show field/file-specific reason |
| `pom_requires_flattening` | 422 | after rebuilding | Explain Maven CI-friendly POM flattening requirement |
| `plugin_prefix_conflict` | 409 | with another prefix | Preserve the existing Maven plugin mapping |
| `redeploy_not_supported` | 422 | no | Explain format immutability; remove overwrite |
| `artifact_conflict` | 409 | Maven confirmation only | Show replace review when `conflict_action=replace` |
| `distribution_filename_reused` | 409 | with a new filename | PyPI cannot rebind an existing/tombstoned file URL |
| `immutable_version_exists` | 409 | with a new version | npm, Cargo, or Go version is already published/tombstoned |
| `crate_name_conflict` | 409 | with a new name | Show case/hyphen/underscore collision |
| `canonical_external_url_required` | 422 | after operator config | Configure stable URL for Cargo registry identity |
| `lifecycle_not_supported` | 409 | no | Hide invalid delete/replace operation and refresh actions |
| `artifact_ownership_conflict` | 409 | no | Require admin cleanup of raw artifact |
| `derived_metadata_not_managed` | 409 | no | Preserve existing third-party metadata; use native tooling/admin cleanup |
| `conflict_plan_expired`, `staged_blob_missing` | 409 | fresh upload | Keep fields, select files again if browser lost handles |
| `concurrent_metadata_update` | 409 | yes | Retry fresh attempt |
| `leadership_changed`, `storage_unavailable` | 503 | yes | Replay idempotency key first |
| `upload_cancelled` | 499 in logs, no response | user choice | Restore editable form |

499 is an internal log/metric status only; the server does not attempt to write
it after the client disconnects.

## Frontend design

Frontend additions:

```text
web/src/routes/workspace/repositories/$id/upload.tsx
web/src/components/artifacts/upload-dropzone.tsx
web/src/components/artifacts/upload-progress.tsx
web/src/components/artifacts/upload-result.tsx
web/src/components/artifacts/upload-forms/maven.tsx
web/src/components/artifacts/upload-forms/pypi.tsx
web/src/components/artifacts/upload-forms/npm.tsx
web/src/components/artifacts/upload-forms/cargo.tsx
web/src/components/artifacts/upload-forms/go.tsx
web/src/lib/artifact-upload.ts
```

`artifact-upload.ts` owns the manifest tagged union, `XMLHttpRequest`, progress,
abort, structured error parsing, and response types. Format components own only
form state and client-side validation.

The client generates the idempotency key when Upload is first pressed and keeps
it for transport retry only. It stores `{repoId, uploadId, idempotencyKey}` in
`sessionStorage` after receiving an upload ID; it never stores package bytes,
CSRF tokens, or manifests there. Add a status endpoint:

```http
GET /api/v1/repositories/{id}/uploads/{uploadID}
```

Only the original principal or an administrator may read it. It returns
`receiving`, retained conflict details, committed result, or failed/expired
state. Status and cancellation also re-check repository read access and source
IP ACL; Maven replacement confirmation re-checks write/delete. On page reload the route restores
committed/conflict UI from this endpoint. A still-receiving request is polled
with capped 1s/2s/5s backoff for
at most one minute. A failed attempt returns to the form; browser security means
the user may need to reselect files.

Use TanStack Query for repository/status/result reads and invalidate repository
detail, artifact list, repository list aggregates, and global search after
commit or publication delete. File and progress state stays local to the route;
large `File` objects must never enter query cache or global context.

Navigation during `receiving` opens the app's confirmation dialog. Confirming
navigation aborts XHR and leaves the form route. Browser reload/close relies on
request cancellation and idempotency recovery; do not use a custom
`beforeunload` string. Navigation from a retained conflict asks whether to
cancel/release it or keep it for its remaining TTL.

Use the existing `Card`, `Field`, `Input`, `Button`, `Alert`, `Badge`, and page
header components. The drop zone must also be a visible keyboard-operable file
input; drag-and-drop is an enhancement. Announce progress and completion through
an `aria-live` region without updating it on every byte. Focus the first invalid
field or error summary after submission.

Add Korean and English strings together. Avoid hard-coded upload text in the
route so the new flow does not repeat the current mixed-localization issue in
the repository detail page.

Accessibility acceptance:

- The drop zone contains a real labeled `<input type="file">`; it is not a div
  pretending to be a button.
- File rows and repeated Maven fields have stable IDs and numbered legends.
- Client and server errors render in one `role="alert"` summary linked to field
  messages with `aria-describedby`; focus moves to the summary once.
- Progress uses `role="progressbar"` with numeric values and a separate polite
  live message announced no more than every 10 percent or 10 seconds.
- Conflict acknowledgement is a required checkbox, not an implicit destructive
  button click.
- Success focus moves to the result heading. All icon buttons have translated
  accessible names and 44px touch targets on mobile.

## OpenAPI changes

- Add `capabilities` and `publish_methods` to `Repository` and repository list
  items.
- Add multipart POST to `/api/v1/repositories/{id}/artifacts` with a documented
  manifest schema and binary asset parts.
- Add GET/DELETE on `/api/v1/repositories/{id}/uploads/{uploadID}` for status
  and conflict cancellation, plus POST on the `/commit` child for conflict
  confirmation.
- Add DELETE on `/api/v1/repositories/{id}/publications/{publicationID}` for an
  atomic Maven/PyPI/npm lifecycle deletion and metadata/tombstone reconciliation.
- Add POST on `/api/v1/repositories/{id}/publications/{publicationID}/yank` for
  Cargo yank/unyank. Add publication `actions` and problem `conflict_action` so
  clients never infer lifecycle support from global delete permission.
- Add `ArtifactUploadResult`, `UploadedArtifact`, and `Problem` schemas.
- Add `UploadStatus`, `UploadConflict`, and the five manifest variants under a
  discriminator-based `oneOf`; prohibit additional properties at every level.
- Document format matrix, limits, authorization, overwrite requirements, and
  all status/error codes, CSRF/idempotency headers, replay behavior, conflict
  retention, and durability modes.
- Include complete Maven, PyPI, npm, Cargo, and Go multipart examples, including
  multi-asset Maven/PyPI and optional Go `mod` parts.

## Delivery plan

The workstreams below are implementation order, not separate product-support
tiers. The upload entry point may remain behind one feature flag while slices
land. General availability requires all five format workstreams and their
native-client end-to-end tests.

### Concrete code and migration map

Backend additions:

```text
src/meta/migrations/0024_artifact_publications.sql
src/meta/migrations/0025_artifact_upload_requests.sql
src/meta/migrations/0026_audit_request_detail.sql
src/meta/migrations/0027_group_metadata_cache.sql
src/meta/publication.rs
src/repo/uiupload.rs
src/repo/uiupload_maven.rs
src/repo/uiupload_pypi.rs
src/repo/uiupload_npm.rs
src/repo/uiupload_cargo.rs
src/repo/uiupload_go.rs
src/repo/group_metadata.rs
src/api/uploads.rs
```


`uiupload.rs` is the orchestration facade injected into `api.Handler`.
Format files do not write metadata directly; they return plans. `api/uploads.rs`
owns path-ID resolution and JSON/problem response adaptation only. Existing
protocol handlers reuse format publication helpers where their native request
contains a complete package (npm and PyPI) and keep raw-path behavior otherwise.
`src/repo/pypi.rs` also gains hosted root Simple API synthesis and complete
PEP 691 1.1 fields. `src/repo/cargo.rs` stops advertising `api` and emits
the correct `auth-required`. Maven/Cargo/Go raw PUT and raw delete reject
managed paths.

Migration requirements:

- Each migration is additive and restart-safe through the existing migration
  runner; no table rewrite of artifact bytes is required.
- `publication_id` remains nullable and existing rows stay null.
- Migration 0024 creates publications and tombstones together because lifecycle
  enforcement must be present from the first managed publication.
- Backfill is not required for launch. A later optional leader job may infer
  publications, but it must never guess across ambiguous Maven classifiers or
  raw paths.
- Rolling upgrade is supported only with the feature flag off until every pod
  runs the new binary. Old binaries ignore nullable columns/tables; the new
  leader owns uploads.
- Downgrade after migrations is data-compatible for reads, but publications
  uploaded by the new version lose component-aware deletion in an old binary;
  document this and require disabling UI upload before downgrade.

The upload endpoint uses the new structured `Problem` writer locally. Do not
change every existing API error in this feature, but put the shared type in
`src/api` so later endpoints can migrate without a second schema.

### Workstream 0 — foundation

- Add per-repository capabilities and UI rendering based on `capabilities.upload`.
- Inject the uploader facade into the API handler.
- Implement bounded streaming multipart parsing, structured errors, staging,
  atomic `ApplyArtifactBatch`, overwrite authorization, metrics, and tests.
- Implement format-aware group metadata aggregation, CAS cache/invalidation,
  ETag handling, and member-policy filtering before enabling any format slice.
- Add the upload route, shared layout, progress, abort, error, and result states.

### Workstream 1 — Maven and PyPI

- Implement Maven multi-asset validation, POM parsing/generation, canonical
  paths, and collision UX.
- Implement PyPI wheel/sdist metadata inspection and reuse existing simple-index
  behavior.
- Keep the global feature flag off until the remaining format workstreams land;
  exercise incomplete handlers directly in tests, not through user-visible
  per-format flags.

### Workstream 2 — npm

- Add secure tarball inspection and atomic packument merge.
- Refactor native `npmPublish` onto the same package publication service.
- Verify new and existing versions install after UI upload.

### Workstream 3 — Cargo and Go

- Add derived sparse index for Cargo.
- Add Go `.mod`, `.info`, and `list` generation.
- Verify native clients against hosted and hosted-first group endpoints.

### Workstream 4 — general-availability gate and polish

- Run the complete five-format matrix against hosted repositories and seeded
  hosted-first groups, including restart persistence and scoped-writer RBAC.
- Enable the single public UI-upload feature flag only after this matrix passes.
- Group upload and metadata audit rows by `request_id=upload_id` in the existing
  repository Audit tab.

## Test strategy

### Test-design validation result

The earlier matrix identified the right behaviors, but it was not yet an
executable release contract. The repository currently has broad tests, but
there is no pull-request CI workflow. The container release workflow runs
`cargo fmt --check`, `cargo clippy -- -D warnings`, race tests, and a 70 percent aggregate Go coverage gate only
when `Dockerfile` changes; it builds the web UI but does not run Vitest or
Playwright. The existing Playwright configuration starts only
[Vite](https://github.com/vitejs/vite), targets Chromium, and its sole E2E test
checks the login form without a running Forklift API. These facts are baseline
gaps, not acceptable launch conditions.

This section closes those gaps. UI upload is ready to implement only if the
test harness, fixtures, required CI checks, and traceability artifacts below are
implemented in the same workstreams as the production code. A format handler is
not complete merely because its parser unit tests pass.

### Test suites and ownership

Use the following suite boundaries so failures identify the faulty layer and
the slow suites do not make developers bypass the fast ones:

| Suite | System under test | Dependencies | Required cadence |
| --- | --- | --- | --- |
| Parser/unit | format inspectors, normalization, plan construction, merge functions, UI reducers/validators | in-memory inputs, fake clock/IDs | every PR |
| Metadata/storage integration | SQLite migrations/transactions, filesystem and S3 stores, cache invalidation, sweeper | temporary DB/directories; MinIO for S3 contract | every PR for filesystem and S3 smoke |
| API contract | real router, auth/CSRF, multipart streaming, OpenAPI response validation | temporary filesystem backend and SQLite | every PR |
| Browser E2E | built React UI embedded in the real Forklift binary | real API, SQLite, filesystem store, stub upstreams | every PR, Chromium smoke; full browser matrix nightly |
| Native consumer | UI/API upload followed by Maven, pip, npm, Cargo, and Go consumption | pinned real CLI binaries, hosted and group repositories | every PR on Linux smoke; full matrix nightly/release |
| Native publisher regression | existing Maven, npm, and twine publisher routes | pinned real publisher CLIs | every PR smoke and full matrix before release |
| HA/performance/soak | lease transfer, S3 metadata durability, bounded resources, recovery | MinIO, multiple Forklift processes/pods, controlled runner | nightly and release gate |

Tests own these repository paths; names are part of the implementation
contract rather than suggestions:

```text
src/repo/uiupload_*_test.rs
src/repo/group_metadata_*_test.rs
src/meta/publication_test.rs
src/meta/meta_test.rs
src/repo/testdata/uiupload/<format>/
web/test/features/upload/
web/test/e2e/upload.spec.ts
test/integration/native/<format>/
test/integration/harness/
test/fixtures/README.md
test/toolchains.lock.yaml
test/traceability/ui-upload.yaml
.github/workflows/ci.yml
```

`test/toolchains.lock.yaml` records the exact Go, JDK/Maven, Node/npm,
Python/pip/twine, and Rust/Cargo versions plus container image digests. Test the
oldest supported and current supported client lines in the nightly matrix, with
one pinned representative of each in the PR smoke suite. CI prints all client
versions before execution. Version updates are explicit dependency-maintenance
changes; `latest` tags are forbidden.

### Requirement traceability and release evidence

Every acceptance criterion below receives a stable ID (`AC-UPL-001`, ...).
Every structured problem code, format rule, mutation transition, group merge
rule, and security limit receives at least one test ID in
`test/traceability/ui-upload.yaml`. Each entry records:

```yaml
requirement: AC-UPL-001
tests:
  - package/test/name-or-playwright-title
suites: [api-contract, browser-e2e]
formats: [maven, pypi, npm, cargo, go]
ci_checks: [backend-contract, upload-e2e]
```

CI validates that referenced tests exist, required format cells are non-empty,
and no acceptance criterion is orphaned. Test results publish JUnit, Go and
frontend coverage, sanitized native-client logs, and Playwright trace,
screenshot, and video artifacts on failure. A retry may collect diagnostics,
but the original failure remains visible and a flaky required test cannot be
silently treated as a pass. Critical upload tests cannot be quarantined for
general availability.

Coverage is a backstop, not the completeness definition. Keep the repository's
aggregate Go gate from decreasing and require at least 85 percent statement
coverage for the new upload orchestration, publication transaction, and group
metadata packages. Require at least 80 percent line and branch coverage for the
new frontend upload feature. More importantly, every state transition and error
branch in the traceability file must be exercised even if percentage thresholds
are already met.

### Backend

- Authorization matrix: anonymous, reader, scoped writer, writer outside scope,
  delete-capable writer, admin, and token-scoped principals.
- Repository matrix: hosted/proxy/group, enabled/disabled, allowed/denied IP.
- Multipart fuzz tests: missing/duplicate parts, reordered parts, oversized
  manifest, too many assets, truncated boundaries, cancellation, and invalid
  encodings.
- Archive adversarial fixtures: traversal, absolute paths, symlinks, duplicate
  normalized paths, metadata mismatch, high compression ratio, and excessive
  entries.
- Atomicity fault injection after each blob stage and before/during metadata
  commit; assert zero visible partial artifacts and correct blob references.
- Concurrent same-coordinate uploads; assert exactly one success when overwrite
  is false.
- Per-format golden paths and generated metadata.
- Scanner, license resolver, negative cache, audit, and metric assertions after
  commit only.

Use deterministic clocks, ULID generators, random seeds, and repository IDs.
Every filesystem/SQLite test uses `t.TempDir`; parallel tests use dynamically
allocated ports and independent stores. Primary tests never contact public
registries. Proxy members are deterministic `httptest.Server` implementations
that can emit 200, 304, 403, 404, 429, truncated, malformed, delayed, and 5xx
responses. External-registry smoke tests, if any, are opt-in and never a merge
gate.

#### Transaction, idempotency, and fault-injection contract

The uploader receives test-only interfaces for the clock, ID generation,
`WriteLease`, blob store, and metadata transaction. Deterministic failpoints
exist at all of these boundaries:

1. before and after each blob `Put`;
2. after the staged-lease row is written;
3. before `BEGIN`, after publication insert, and after artifact upsert number N;
4. before and after blob-reference adjustments;
5. before aggregate-cache invalidation;
6. before the upload state becomes `committed`;
7. after commit but before the HTTP response;
8. during cancellation/sweeping and immediately before or after leadership
   loss.

Failpoints are injected through test doubles or a `cfg(test)` feature; production
environment variables cannot activate them. After every injected failure,
restart the store and assert all of these invariants directly from the database
and storage enumerator:

- every visible artifact references an existing blob with the recorded digest;
- reference counts equal live artifact plus retained-cache references and are
  never negative;
- a committed publication owns its complete expected artifact set, while no
  failed/cancelled publication owns a visible artifact;
- terminal requests have no live staging lease except a documented retained
  conflict plan, which expires and is swept;
- an idempotency key maps to one canonical manifest hash and one terminal result;
- a post-commit lost response is recovered as the original result, not a second
  commit;
- aggregate metadata is either the old complete representation or the new
  complete representation, never a partially rebuilt document;
- audit, metrics, scan, and license work appear once and only after commit.

Run model-based state-machine tests across `receiving -> planned/conflict ->
committing -> committed`, cancellation, expiry, replay, and leadership loss.
Generate operation sequences for upload/confirm/cancel/status/delete/unpublish/
yank and compare the real service to a small reference model of each format's
mutation policy. Use property tests for path normalization, version-equivalence
classes, deterministic metadata rendering, and merge precedence. Group merge is
order-sensitive by design, so test stability under a fixed order rather than
commutativity.

#### Migration and mixed-version tests

Migration tests must cover a fresh database; an upgrade snapshot ending at
0023 containing representative repositories, artifacts, users, and audit rows;
repeated startup after all new migrations; foreign-key and uniqueness
constraints; and a restart between every migration. Verify old rows remain
readable with null `publication_id` and are never guessed into a publication.
Verify feature-flag-off operation while old and new binaries coexist, then
feature enablement only after every node is new. A rollback fixture proves that
the old binary can still read artifacts after the schema upgrade and documents
the expected loss of component-aware lifecycle operations. Destructive down
migrations are not required and must not be fabricated.

#### API, protocol, and security contracts

Validate every success payload and `application/problem+json` response against
the generated OpenAPI schema, including content type, status, headers, field
pointers, retry metadata, and unknown-field behavior. Round-trip OpenAPI
examples through the real handlers. Contract cases include cookie and token
authentication, CSRF rotation/expiry, Origin checks, scoped authorization,
request cancellation, `Expect: 100-continue`, chunked bodies, wrong or missing
`Content-Length`, duplicate multipart names, and a disconnect after the last
body byte.

Security fixtures include filename/metadata HTML and terminal injection,
Unicode confusables and normalization edges, JSON/XML control characters,
archive traversal and link variants, duplicate normalized archive paths,
compression bombs, oversized central directories, and malformed nested
metadata. Tests assert logs, problem details, audit rows, and client logs do not
contain passwords, bearer tokens, cookies, CSRF values, or raw private metadata.
Metrics tests reject per-user, repository, package, filename, and upload-ID
labels to prevent cardinality and information leaks.

#### Format-specific backend cases

The shared matrix is supplemented by these format gates:

- **Maven:** POM-only/BOM, generated versus supplied POM, sources/javadoc and
  arbitrary classifiers, signatures/checksums, snapshot rejection, URL-escaped
  group/artifact coordinates, CI-friendly unresolved placeholders, malformed
  or mismatched POMs, plugin-prefix collision/deletion, and metadata containing
  legacy fields. Prove `latest` and `release` follow last-updated Maven rules,
  not semantic-version sorting, and that delete/replace recomputes a valid
  fallback. Race UI replacement against raw `mvn deploy` PUTs.
- **PyPI:** modern and legacy sdist layouts; wheel `METADATA`, `WHEEL`, tag, and
  `RECORD` agreement; normalized-name aliases; epoch/local/pre/post/dev PEP 440
  versions; multiple compatible wheel tags; duplicate and tombstoned filenames;
  sequential multi-file twine requests; and concurrent extension of one
  release. Validate hosted root/project HTML and PEP 691 JSON, `Vary`, hashes,
  sizes, upload times, escaping, and content negotiation.
- **npm:** unscoped/scoped names with literal, `%2f`, and `%2F` route encodings;
  strict semantic versions; prereleases; dist-tag add/move/delete; rejection of
  range-like tags; SHA-1/SHA-512 integrity; unknown safe packument fields;
  corrupted existing packuments; generated tarball URLs; tombstones; and
  UI/native publish/unpublish races in both orders.
- **Cargo:** crate-name case and hyphen/underscore collisions; build-metadata
  identity; renamed/optional/target/registry dependencies; crates.io-versus-
  alternate-registry encoding; null fields; `features`, `features2`, schema
  version, `rust_version`, and publication time; sparse-index append order;
  yank/unyank changing only the intended line; raw-token auth and 401 challenge;
  and `config.json` with `auth-required:true` and no publish `api`.
- **Go:** uppercase path escaping, semantic import version `/v2`,
  `+incompatible`, prerelease and pseudo-version timestamps, canonical zip root
  and forbidden zip contents, supplied/generated `.mod`, `list` exclusion of
  pseudo-versions, `@latest` precedence, proxy URL escaping, and byte-for-byte
  immutability after every rejected overwrite/delete attempt.

Fuzz targets cover multipart parsing and every archive/metadata decoder. Keep a
small reviewed seed corpus and all minimized regressions in `testdata`; ordinary
PR tests always replay it. Scheduled jobs run time-bounded `cargo fuzz`
campaigns per target and publish crashers. A fuzz timeout or resource limit is a
failure, not an ignored input.

### Frontend

- Capability-driven button visibility for scoped writers, readers, proxy/group,
  and disabled repositories.
- Required-field, duplicate-asset, type, count, and size validation.
- Progress, abort, retry, conflict/replace, structured field errors, and success
  navigation.
- Keyboard-only and mobile viewport tests.
- English/Korean snapshots for all states.

Prefer role/name/state assertions over large DOM snapshots; snapshots cover only
stable format summaries and localized copy. Component tests use fake files and a
controllable XHR adapter to test monotonic progress, zero/unknown totals,
abort-before-send, abort-during-send, timeout, network loss, and response-after-
abort races. Test file re-selection, duplicate display names, removal/reorder,
manifest changes after validation, double submit, expired CSRF, 409 plan expiry,
409 replace versus non-replace choices, 413/422 field focus, 429 retry timing,
reload recovery from `sessionStorage`, and cleanup after success/cancel.

Automated accessibility tests inspect every step and error/conflict state for
labels, focus order, focus restoration, modal trapping, keyboard operation,
live-region announcements, contrast, reduced motion, and 200 percent zoom.
Filename and server-provided text are rendered as text, never HTML. Test Korean
and English with long filenames and long localized errors at desktop and narrow
mobile widths.

Once upload tests land, remove `passWithNoTests: true`. Chromium is the PR
browser; Chromium, Firefox, and WebKit plus desktop and narrow viewports run
nightly. A browser retry is diagnostic only: CI still records the initial
failure as flaky and applies the flake policy above.

### End to end

#### Real-service harness

Browser E2E must not mock the upload endpoint. Build the web bundle, build one
Forklift binary, start it with a unique temporary data directory, a fixed test
administrator, UI upload enabled, and an OS-assigned port, and wait on the real
health/version endpoint. Serve the embedded production UI from that binary so
routing, cookies, CSRF, request limits, and static asset integration match the
release artifact. The harness creates scoped users and repositories through the
API, captures sanitized process logs, and terminates the process on test exit.
No test relies on a developer's process on ports 5173 or 8080.

Use seeded local upstream servers for group tests. The upstream records requests
and can switch status, delay, ETag, and representation without the internet. S3
contract E2E runs the same binary against an ephemeral pinned
[MinIO](https://github.com/minio/minio) instance. HA tests start at least two
Forklift processes over the same configured metadata/storage topology, force
lease-owner termination at each commit boundary, and assert the configured RPO
and replay behavior. A lightweight fake-lease test runs on PRs; real
multi-process failover runs nightly and before release.

For each format, upload through the UI endpoint and install through the native
client using the hosted repository. Also install through the seeded hosted-first
group to confirm derived metadata and URLs are correct. Restart the service and
repeat the install to verify persisted metadata and blob references.

Run the native publisher regression matrix separately: `mvn deploy:deploy-file`,
`npm publish`, and `twine upload` must still succeed against hosted repositories.
For npm and PyPI, publish through CLI then collide/extend through UI and repeat
in the opposite order to prove both decoders share one publication policy.
Assert Cargo `config.json` omits `api` and a `cargo publish` attempt fails with a
clear unsupported capability rather than a misleading storage error. Tests
must not label raw PUT as a native publisher.

Native clients run with isolated home/cache/config directories and credentials
scoped to the temporary repository. They may access only the test server and
local tool caches. Maven settings, npm user config, pip config, Cargo config,
and Go environment are generated per test and sanitized before artifact upload.
Seed every transitive dependency and plugin needed by the fixtures into the
temporary hosted/proxy members; client configuration has no public fallback and
tests run offline after tool installation. Go cases set test-scoped
`GOPRIVATE`/`GONOSUMDB`, and checksum-database or VCS traffic is a failure.
Use CLI exit status plus an artifact-level assertion: downloaded bytes/digests,
resolved dependency graph, selected version/tag, and request log. A command
that exits zero without fetching the intended Forklift URL is not a pass.

The PR smoke matrix uses one minimal valid fixture per format plus all three
supported native publisher regressions. Nightly executes all fixture variants,
oldest/current supported clients, hosted/group paths, authentication modes, and
cross-surface races. The release candidate executes the nightly matrix on the
actual container image rather than a source-built binary.

The minimum matrix is:

| Format | Upload fixture | Native verification |
| --- | --- | --- |
| Maven | Generated POM plus binary and sources JAR | `mvn dependency:get` for the exact GAV and `mvn dependency:resolve` through the group |
| PyPI | Modern and legacy sdist plus multiple wheel tags, including a later extension of the same release | Negotiate JSON/HTML, then `pip install project==version` for compatible wheel and sdist-only environments |
| npm | Unscoped and scoped tarballs with stable/prerelease versions and non-semver tags | `npm view`, tagged `npm install`, and exact-version install; verify overwrite and post-unpublish reuse fail |
| Cargo | Crate with renamed, optional, target-specific, registry, and extended-feature dependencies | Sparse-index resolution and fetch/build; verify duplicate/build-metadata identity fails and yank preserves lockfile fetch |
| Go | Release, pre-release, and pseudo-version module zips, including an uppercase module path | Verify `list` excludes pseudo-versions, then `go mod download`/`go get`; verify overwrite/delete are unavailable |

Additional compatibility gates: invoke a UI-uploaded Maven plugin by goal
prefix; fetch PyPI `/simple/` and project pages with both standardized Accept
types and validate the PEP 691 schema; run authenticated Cargo against
`auth-required:true` and confirm no publish `api` is advertised; compare every
Go `.mod`/`.zip` digest before and after rejected lifecycle attempts.

For every format, seed the same package with one version in hosted and a
different version in proxy, then resolve both through the group. Assert the
group index exposes the union, a duplicate coordinate/file follows configured
member precedence, generated URLs point back to the group, ETag returns 304,
and a hosted upload/delete/yank invalidates the cached union immediately.

Also exercise a member 403/404/429/5xx, timeout, malformed metadata, conflicting
duplicate, configuration revision, and member reorder. Assert the documented
failure policy, that unauthorized or policy-denied versions never leak through
metadata, that `Accept` variants have distinct valid cache entries, that source
TTL and conditional requests behave correctly, and that concurrent cache misses
single-flight without changing precedence. Test maximum allowed aggregate
metadata and verify over-limit behavior is a stable problem rather than a
truncated valid-looking document.

### Fixtures and golden-data policy

Generate minimal valid packages from readable source projects under
`test/integration/native/<format>` whenever possible. Check in only small binary
or adversarial inputs that cannot be generated cheaply. Every checked-in fixture
is listed in `test/fixtures/README.md` with origin, license, generator command,
expected digest, intended assertion, and whether malformed content is
deliberate. Golden metadata is canonicalized before comparison; intentional
changes require a focused review of the rendered diff. Timestamps, archive
entry order, permissions, and compression are fixed so reruns are byte-
reproducible.

Never derive expected values with the same production helper being tested.
Where a standard client can be the oracle, retain its parsed result and request
trace; where it cannot, use independently written table expectations. Archive
bombs are represented by compact generators and limits, not enormous committed
files.

### Required CI and release gates

Add `.github/workflows/ci.yml` for `pull_request` and pushes to `main`. The
following named checks are required before merge:

1. `backend-static`: generated-file cleanliness, `cargo fmt --check`, `cargo clippy -- -D warnings`, OpenAPI
   generation/diff, and migration ordering.
2. `backend-test`: Rust unit/integration tests with concurrency regressions, deterministic
   seed-corpus replay, aggregate and new-package coverage gates.
3. `web-test`: frozen pnpm install, Vitest with coverage, TypeScript/build, and
   generated-query cleanliness.
4. `backend-contract`: real-router API/OpenAPI tests plus filesystem and MinIO
   storage contract smoke.
5. `upload-e2e`: real embedded Forklift service, Chromium, one package per
   format, hosted/group install, restart, and Maven/npm/twine publisher
   regression.
6. `traceability`: acceptance/problem/format matrix completeness and no skipped
   or focused required test.

The existing container-release workflow must depend on or repeat the exact
commit's successful required checks; a `Dockerfile`-only trigger cannot remain
the sole test gate. Nightly CI adds all browsers, full pinned client matrix,
fuzz campaigns, S3 fault cases, and multi-process HA. Release CI additionally
runs the actual image matrix, controlled performance suite, and 24-hour soak.
Required jobs upload diagnostic artifacts even on failure and use explicit
timeouts. Secrets are least-privilege test credentials and are scrubbed from
logs.

No required matrix cell may use conditional skip because a CLI, browser, or
MinIO is missing: CI images install the pinned dependencies up front and fail
fast during environment validation. Local developer commands may select a
subset, but `make test-upload`, `pnpm test:e2e`, and the CI jobs share the same
harness and fixture definitions to prevent two test implementations drifting.

### Performance and soak tests

- Four concurrent maximum-size uploads on filesystem and S3 backends; assert
  process RSS stays within the configured container memory limit with bounded metadata
  overhead and no task/file-descriptor leak.
- 100 concurrent small uploads distributed across packages and users; assert
  semaphore fairness, 429 behavior, unrelated package parallelism, and SQLite
  commit latency.
- Repeated same-package uploads and format-valid lifecycle changes while native clients read indexes;
  clients must observe either the complete old state or complete new state.
- Cancel at every 5 percent of receive and at each injected plan/commit boundary;
  assert no publication rows and eventual staged-blob reclamation.
- 24-hour soak with periodic S3 metadata snapshots, leader failover, conflict
  plan expiry, sweeper runs, and vulnerability/license queue saturation.

Measure receive, validation, plan, commit, metadata regeneration, and client
resolution separately. Record peak RSS, heap allocation, active tasks, open file
descriptors, staging bytes, SQLite lock/commit latency, S3 requests, throughput,
and p50/p95/p99 latency. Before and after each test, assert process/thread/file
descriptor counts return to a bounded baseline and enumerate residual staging
leases/blobs.

Absolute latency and RSS budgets run only on a documented controlled runner
with fixed CPU/memory limits, local-SSD class, backend configuration, package
corpus, warm-up, sample count, and allocator settings. Shared PR runners use
allocation-focused benchmarks and invariant/leak checks rather than flaky wall-
clock gates. Store a versioned baseline and require review for a statistically
meaningful regression; do not silently rewrite it. The filesystem and S3
variants use identical logical workloads. The 24-hour run periodically verifies
native reads and database/storage invariants, not merely process survival.

Performance release budgets with default limits:

- Metadata-only validation memory: at most 32 MiB per active request, excluding
  storage backend streaming buffers and ZIP central-directory structures.
- Small package (<10 MiB) server validation plus commit p95: <2s after body
  receipt on local SSD, excluding external scan work.
- Artifact-list refresh visible within one successful response round trip.
- No unbounded cardinality labels: metrics use format/result only, never
  repository, package, user, or upload ID where the metric family is global.

## Rollout and operations

1. Deploy migrations and code to every replica. UI upload is enabled by
   default; set `FORKLIFT_UI_UPLOAD_ENABLED=false` only when a staged rollout or
   emergency opt-out is required. Verify leader routing, storage capacity, and
   migration health.
2. Run the five-format E2E matrix against a non-production hosted repository on
   the deployed binary before allowing production publishers to use the UI.
3. Configure Ingress size/timeouts and alerting before production enablement.
4. Confirm that capabilities expose the button in production; no values change
   or web asset redeploy is needed beyond the same release.
5. During rollback, disable the flag first, wait for `receiving` uploads to
   finish or cancel, then deploy the prior binary. Retained conflict plans may
   be cancelled safely; their blobs are swept.

Recommended alerts:

- `rate(forklift_ui_uploads_total{result="error"}[10m])` above an absolute and
  percentage threshold.
- sustained `upload_busy`/429 count, indicating concurrency or proxy tuning.
- staged conflict bytes/plan count approaching staging capacity.
- oldest `receiving` upload older than `MaxDuration + 5m`.
- zero-reference blob bytes growing without a successful sweeper run.
- HA metadata last-sync age above twice its configured interval; successful UI
  uploads in asynchronous durability modes are then at increased failover risk.

Admin status must expose current receiving/conflict counts and staged bytes on
the Storage page. Do not expose package filenames or uploader identities in
[Prometheus](https://github.com/prometheus/prometheus); those remain in
authenticated audit views.

Operator documentation must include Ingress annotations/examples for the
supported chart controller, S3 staging-volume sizing, private Go
`GOPRIVATE`/`GONOSUMDB` setup, all five native publishing alternatives, and the
asynchronous HA durability warning.

## Acceptance criteria

- **AC-UPL-001 — Scoped upload:** A repository-scoped writer, not only an administrator, can see and use the
  upload action for an enabled hosted repository.
- **AC-UPL-002 — Authorization:** The same user cannot upload to a repository outside their `write` scope or
  from an IP denied by the repository ACL.
- **AC-UPL-003 — Repository capability:** Proxy, group, and disabled repositories reject uploads server-side even if the
  endpoint is called directly.
- **AC-UPL-004 — Atomic visibility:** No failed or cancelled request exposes a partial component.
- **AC-UPL-005 — Maven replacement:** Maven paths are never replaced without explicit confirmation and `delete`
  permission; confirmation reuses retained blobs and does not upload them again.
- **AC-UPL-006 — Immutable identities:** PyPI extends a release only with new non-tombstoned filenames; npm coordinates,
  Cargo entries, and Go module versions are never rebound to different bytes.
- **AC-UPL-007 — Idempotency:** Idempotency replay returns the original committed result after a lost response
  and never commits the same attempt twice.
- **AC-UPL-008 — CSRF:** Cookie-authenticated receive/confirm/cancel rejects a missing or invalid CSRF
  token.
- **AC-UPL-009 — Bounded streaming:** Files are streamed with the documented limits and no dependency on container
  `/tmp`; S3 uses only its explicitly sized private staging directory.
- **AC-UPL-010 — HA recovery:** Leadership loss before commit exposes no artifact; a post-commit lost response
  is recoverable through upload status/idempotency within the configured HA RPO.
- **AC-UPL-011 — Lifecycle:** Maven/PyPI/npm deletion or unpublish atomically reconciles metadata and writes
  required tombstones; Cargo yank changes only `yanked`; Go exposes no mutable
  publication lifecycle.
- **AC-UPL-012 — Native consumption:** Every format's package is consumable by its native client after upload and
  after service restart.
- **AC-UPL-013 — Group aggregation:** A hosted-first group exposes the union of hosted and proxy metadata for the
  same package in all five formats while immutable file bytes retain first-member
  precedence; managed mutations invalidate the aggregate before returning.
- **AC-UPL-014 — Observability:** Successful artifacts show the correct uploader, audit rows, ingress metrics,
  durability, and queued/deferred vulnerability/license analysis.
- **AC-UPL-015 — Capacity:** Maximum-size concurrency stays within staging-capacity and memory budgets;
  excess work receives stable 429 responses.
- **AC-UPL-016 — Shipped verification:** OpenAPI, Korean/English UI copy, backend tests, frontend tests, and end-to-end
  package installation tests ship with the feature.
- **AC-UPL-017 — Publisher compatibility:** Existing `mvn deploy`, `npm publish`, and `twine upload` workflows pass
  regression tests; npm/PyPI cross-surface collisions produce identical policy
  results, while Cargo and Go accurately report the absence of a supported
  native publish command.
- **AC-UPL-018 — Required gates:** All six required PR checks pass for the exact
  commit with no skipped format cell, focused test, orphaned acceptance
  criterion, or silently accepted flaky test; the full nightly matrix passes
  before the feature flag is enabled for general availability.
