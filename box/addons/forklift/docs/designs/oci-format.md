# OCI registry format

## Status

**Implementation status: Implemented.** Verified on 2026-08-19 against `main`
commit `81763cd` (working tree). The distribution API lives in
`src/repo/oci.rs` (routing, pull, tags), `src/repo/oci_push.rs`
(sessions, manifest verification), `src/repo/oci_proxy.rs` (bearer-token
handshake, Docker Hub library prefix), and `src/repo/oci_prune.rs`
(reachability GC and session expiry); tags and sessions persist via
`src/meta/oci.rs` (migrations 0030/0031). Verified end to end with crane
(push, pull, index), helm (push, pull) and podman (push, pull) plus a live
Docker Hub proxy pull, and the official OCI distribution-spec conformance
suite v1.1.1 passes all four workflows (pull, push, content discovery, content
management): 74 passed, 0 failed. The suite runs in CI on every pull request
(the `oci-conformance` job in `.github/workflows/ci-forklift.yml`). Three deviations from the text below: an
unauthenticated `GET /v2/` always answers 401 with the Basic challenge (a 200
makes docker and podman drop their credentials); upstream 401/403 responses
are relayed as 404 because public registries use them for absent images; and
the referrers API (end-12), a non-goal below, was implemented after the
conformance suite required it — including OCI-Subject on push and
referrer-aware liveness in the prune.

Accepted for incremental implementation. This document defines the contract for
a sixth repository format, `oci`, that speaks the OCI Distribution
Specification v1.1 so container images, Helm charts and other OCI artifacts are
hosted, proxied and grouped by Forklift the way the five package formats
already are.

Normative sections use **must** semantics even where explanatory prose says
"should"; changes to routing, storage layout, garbage collection or
authentication behavior require updating this design before merge.

## Overview

Forklift serves Maven, npm, Cargo, Go modules, PyPI and raw
(`src/meta/models.rs:193`). All five package formats are build-time
dependency sources. The runtime artifacts a Kubernetes cluster actually pulls —
container images and Helm charts — have no home in Forklift, so a cluster that
uses Forklift for dependencies still needs a second registry for images.

This design adds the `oci` format: one route family (`/v2/`), one storage layout
on the existing content-addressed blob store, and the hosted/proxy/group triad
every other format already supports.

Read this before touching `/v2` routing, the OCI storage layout, upload
sessions, or garbage collection of manifest-referenced blobs.

## Context

Two properties of the existing implementation make OCI cheaper here than a
sixth package format would normally be:

- Storage is already content-addressed by SHA-256. `storage.BlobStore.Put`
  returns the digest of what it stored (`src/storage.rs:86`,
  `src/storage/s3.rs:133`), and reference counting lives in the `blobs`
  table (`src/meta/migrations/0001_init.sql:14`). The OCI blob model is the
  same model with the digest supplied by the client instead of computed by the
  server.
- The request path is already format-agnostic below the handler.
  `Engine.serve`/`Engine.put` (`src/repo.rs:261`, `:737`) handle
  cache freshness, proxy fetch, audit, metrics and the store hooks; the policy
  pipeline is invoked uniformly with `(pkg, version)` strings
  (`src/repo/policypipeline.rs:22`).

Three properties make it harder than the package formats:

- Push is a multi-request session protocol (`POST` → `PATCH`… → `PUT`), not a
  single `PUT` of one file. No existing format needs server-side session state.
- A manifest is a metadata document that *references* blobs. Forklift's blob
  liveness is per artifact row; nothing today understands that deleting one
  artifact row can break a different, still-live artifact.
- Public upstreams (Docker Hub, GHCR) require a per-scope bearer-token
  handshake. `UpstreamAuthConfig` (`src/repoconfig.rs:51`) only
  carries static credentials.

## Goals

- A hosted `oci` repository accepts `docker push` / `helm push` / `oras push`
  and serves `docker pull` unchanged, with no client-side plugin.
- A proxy `oci` repository caches from Docker Hub, GHCR, Quay and any
  spec-compliant registry, including their token handshakes.
- A group `oci` repository resolves manifests and blobs by member priority and
  merges tag lists.
- Existing RBAC, source-IP ACL, audit, approval, version-deny and age policies
  apply to OCI pulls with no new policy code.
- No stored image can be broken by garbage collection.

## Non-goals

- Image vulnerability or license scanning. OSV and deps.dev have no ecosystem
  for container images; the gates no-op for the format (see
  [Supply-chain policies](#supply-chain-policies)).
- The referrers API (`end-12`) and the OCI 1.1 subject/artifactType graph.
- Image signature verification (cosign, Notation) and admission policy.
- Manifest conversion or rewriting: Docker schema 2 and OCI manifests are stored
  and served byte-identically. Schema 1 manifests are rejected.
- Cross-registry replication or mirroring beyond the existing proxy cache.
- Declaring OCI repositories through Helm values or GitOps, matching the scope
  limit in [Policy evaluation pipeline](policy-pipeline.md).

## Decision summary

Add `meta.FormatOCI = "oci"` and register the distribution API at the router
root as `/v2/{repo}/*`, with the first path segment after `/v2/` naming the
Forklift repository and the remainder forming the OCI repository name. Blobs and
manifests are stored as ordinary artifact rows whose path encodes the digest, so
reference counting, replication and the sweeper work unchanged. Tags live in a
new `oci_tags` table because they are mutable pointers, which the artifact table
deliberately does not model. Push sessions are persisted in a new
`oci_upload_sessions` table with their bytes in the shared data directory, so a
push survives being load-balanced across replicas.

Garbage collection is the part that must not be approximated. In v1 the idle
retention reaper and the cache-size eviction path skip `oci` repositories
entirely, and a new manifest-aware prune computes the live blob set from stored
manifests. Shipping OCI with the existing LRU eviction pointed at it would
delete layer blobs out from under tagged manifests.

## URL layout and routing

`docker pull` derives the request path from the image reference: a reference
`host/a/b/c:tag` produces `GET /v2/a/b/c/manifests/tag`. The client will not
accept a path prefix ahead of `/v2`, so the format cannot live at
`/oci/{repo}/…` like the others. The Forklift repository name is therefore the
first segment of the image name:

```
docker pull forklift.example.com/oci-public/library/nginx:1.27
                                 ^^^^^^^^^^ ^^^^^^^^^^^^^^
                                 repository  OCI name
```

`Manager.Register` (`src/repo/router.rs:230`) gains:

The Axum registry router mounts `/v2`, `/v2/`, and `/v2/{repo}/{*rest}`
inside the authenticated route group.

Routing must satisfy:

- `GET /v2/` returns 200 with `Docker-Distribution-API-Version: registry/2.0`
  and an empty JSON object, or 401 with a `Basic` challenge when anonymous read
  is off. It resolves no repository.
- `/v2/{repo}/*` splits the wildcard on the last two segments to recover the
  endpoint: `…/blobs/{digest}`, `…/manifests/{reference}`,
  `…/tags/list`, `…/blobs/uploads/`, `…/blobs/uploads/{session}`. Everything
  left of that suffix is the OCI name, which must match
  `[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*` per the spec.
- A path that matches no endpoint returns 404 with code `NAME_UNKNOWN`, never
  the SPA fallback (`src/bin/forklift.rs:421`).
- The mount point stays inside the authenticated group
  (`src/bin/forklift.rs:413`), so handlers see the principal.

Endpoint coverage for v1 is `end-1` through `end-11` of the distribution spec,
including cross-repository blob mount (`end-11`, `?mount=&from=`), which BuildKit
and `crane copy` rely on to avoid re-uploading shared layers. `end-12`
(referrers) returns 404 with `UNSUPPORTED`.

### Error responses

The spec requires errors as `{"errors":[{"code":…,"message":…}]}` with
`Content-Type: application/json`. `resolveRepo` writes plain-text errors
(`src/repo/router.rs:334`) and is shared by every format, so the OCI
handler wraps its response writer in an `ociErrorWriter` that converts any
non-2xx plain-text body into the JSON envelope, mirroring the existing
`groupWriter` (`src/repo/group.rs:119`) and `statusWriter`
(`src/repo/router.rs:287`) wrappers. Codes used: `BLOB_UNKNOWN`,
`BLOB_UPLOAD_INVALID`, `BLOB_UPLOAD_UNKNOWN`, `DIGEST_INVALID`,
`MANIFEST_BLOB_UNKNOWN`, `MANIFEST_INVALID`, `MANIFEST_UNKNOWN`, `NAME_UNKNOWN`,
`SIZE_INVALID`, `UNAUTHORIZED`, `DENIED`, `UNSUPPORTED`, `TOOMANYREQUESTS`.

## Storage model

Blobs and manifests are immutable and digest-addressed, so they map onto the
`artifacts` table (`src/meta/migrations/0001_init.sql:21`) with no schema
change. Paths are name-scoped:

| Object | Artifact path | `version` | `content_type` |
|---|---|---|---|
| Blob | `{name}/blobs/sha256/{hex}` | `""` | from the manifest layer entry, else `application/octet-stream` |
| Manifest | `{name}/manifests/sha256/{hex}` | `""` | the manifest media type as pushed |

Name-scoping is deliberate even though the blob store deduplicates bytes
globally: it keeps per-image listing, deletion and audit meaningful, and one
row per (name, digest) is what makes `blobs.ref_count` correct when one image is
deleted and another still uses the same layer.

Tags are mutable and must not be artifact rows. Migration
`0030_oci_tags.sql` (successor to `0029_announcement.sql`):

```sql
CREATE TABLE oci_tags (
    repo_id         INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    tag             TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (repo_id, name, tag)
);
CREATE INDEX idx_oci_tags_manifest ON oci_tags(repo_id, name, manifest_digest);
```

Rules:

- A tag write is a single upsert of one row, so re-tagging is atomic and never
  leaves a tag pointing at nothing.
- Resolving `…/manifests/{tag}` reads `oci_tags`, then serves the manifest
  artifact by digest. Resolving `…/manifests/{digest}` skips the tag table.
- On a proxy, the tag row is cached metadata and revalidates on
  `Cache.MetadataTTL` (`src/repo.rs:821`), because a tag upstream is
  a moving target. Manifests and blobs fetched by digest are `kindArtifact` and
  never revalidate.
- `HEAD` on a manifest must return the same `Docker-Content-Digest`,
  `Content-Type` and `Content-Length` as `GET`, with no body. `docker pull` uses
  this for its up-to-date check.

Manifests are size-capped before parse (`fetchSpec.maxStoreBytes`,
`src/repo.rs:239`) at `FORKLIFT_OCI_MAX_MANIFEST_BYTES`, default
4 MiB.

## Push

`PUT …/manifests/{reference}` is the commit point of a push, and it must be
verified, not merely stored:

1. Parse the manifest against its declared media type. Reject an unknown or
   schema 1 media type with `MANIFEST_INVALID`.
2. For an image manifest, every `config` and `layers[]` descriptor must already
   have a stored blob row for this `(repo_id, name)`. A missing one returns
   `MANIFEST_BLOB_UNKNOWN`. For an index, every referenced manifest must be
   present. This is what makes "the tag exists" imply "the image is pullable".
3. Store the manifest bytes, then upsert the tag row when the reference is a
   tag rather than a digest. Both happen in one transaction.
4. Respond 201 with `Location` and `Docker-Content-Digest`.

Blob upload sessions require server state that no other format needs:

```sql
CREATE TABLE oci_upload_sessions (
    id         TEXT PRIMARY KEY,          -- opaque session id, also the temp file name
    repo_id    INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    offset     INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

- Session bytes are appended to a file under the data directory's temp path (the
  same directory already passed to `NewS3BlobStore`,
  `src/storage/s3.rs:95`), so with an RWX volume or the S3 backend any
  replica can continue a session another replica started. This is the reason the
  offset lives in the metadata store rather than in memory: a push must not
  break because the ingress sent `PATCH` to a different pod than `POST`.
- `PATCH` without `Content-Range` appends and returns 202 with
  `Range: 0-{offset-1}`. `PATCH` with a `Content-Range` whose start does not
  equal the stored offset returns 416, per spec.
- `PUT …/blobs/uploads/{session}?digest=` finalizes: the assembled bytes go
  through `BlobStore.Put`, and the returned digest must equal the client's
  digest or the upload is discarded with `DIGEST_INVALID`. Verification is
  against what was actually stored, never against a digest the client asserts.
- Monolithic `POST …/blobs/uploads/?digest=` is supported and skips the session
  table entirely.
- Sessions older than `FORKLIFT_OCI_UPLOAD_SESSION_TTL` (default 24h) are
  deleted with their temp files by the leader-gated prune below.

Pushes are subject to the existing RBAC mapping: `actionForMethod`
(`src/repo/router.rs:208`) already maps `POST`/`PUT` to write and `DELETE`
to delete, so `POST`, `PATCH` (added to the write case) and `PUT` all require
write on the repository.

## Proxy

The pull path reuses `Engine.serve` with `fetchSpec.upstreamURL` built from the
repository's upstream. Two format-specific behaviors are new:

**Upstream token handshake.** On a 401 whose `WWW-Authenticate` is
`Bearer realm=…,service=…,scope=…`, the engine performs a `GET` against the
realm with the scope, using the repository's configured
`UpstreamAuthConfig` basic credentials when present and anonymously otherwise,
then retries with the returned token. Tokens are cached in memory keyed by
`(repo_id, scope)` until expiry, honoring `expires_in`. This is required for
Docker Hub, GHCR and Quay; without it every proxy pull 401s.

**Docker Hub official images.** Docker Hub resolves `nginx` as `library/nginx`,
a normalization the client performs only for the `docker.io` hostname. A proxy
repository whose upstream host is `registry-1.docker.io` prefixes single-segment
names with `library/` when `Upload.OCIDockerHubLibraryPrefix` is set, which
defaults to true for that host and false otherwise. Format-scoped config flags
have precedent in `Upload.PyPIAllowLegacyZIP`
(`src/repoconfig.rs:135`).

Rate limiting matters more here than for the package formats: an upstream 429
must be surfaced as `TOOMANYREQUESTS` with the upstream `Retry-After`
preserved, which `Engine.writeRetry` (`src/repo.rs:573`) already
does.

## Group repositories

Manifest and blob requests are immutable lookups and use the existing streaming
first-hit fan-out in `grouped` (`src/repo/group.rs:47`), so member priority
resolves duplicate names.

Tag lists are mutable indexes and must merge, joining the existing mechanism:

- `groupMetadataKind` (`src/repo/group_metadata.rs:59`) returns `oci-tags`
  when the format is `oci` and the wildcard ends in `/tags/list`.
- `groupMetadataContentType` (`:223`) maps `oci-tags:default` to
  `application/json`.
- `mergeGroupMetadata` (`:284`) dispatches to `mergeOCITagsGroup`, which takes
  `name` from the first successful member and returns the union of `tags`,
  lexically sorted. A `name` mismatch between members is an error, mirroring
  `mergeMavenGroup`'s identity check (`:307`).
- `n`/`last` pagination is applied after the merge, and the `Link` header is
  synthesized from the merged list.

`ValidateGroupMembers` (`src/repo/group.rs:157`) needs no change; it
already requires format equality.

## Supply-chain policies

The pipeline is invoked with a package coordinate and a version
(`src/repo/policypipeline.rs:22`). For OCI:

| Coordinate | Value |
|---|---|
| `pkg` | the OCI name, e.g. `library/nginx` |
| `version` | the tag when the request addresses a tag, otherwise the digest |

What this yields, with no new policy code:

- **Approval** (`src/repo/approvalgate.rs:41`) and **version deny** work on
  `name` + `tag`, which is the natural review unit for an image.
- **Age policy** works on a proxy from the upstream response's `Last-Modified`
  or, when absent, the config blob's `created`. Digest-addressed manifests carry
  no publication date of their own.
- **Source-IP ACL** and **RBAC** apply through the shared `resolveRepo` and
  `authorize` path.
- **Vulnerability and license gates no-op by construction.** `osvEcosystem`
  (`src/repo/vulnscan.rs:133`) and `depsDevSystem`
  (`src/repo/licensescan.rs:41`) return `""` for an unmapped format, and
  `vulnGate` returns before evaluating `block_unscanned`
  (`src/repo/vulngate.rs:22`). No exemption code is needed, but the
  repository Security tab must state that the two gates are inert for `oci` so
  an operator does not read an enabled-but-inert policy as coverage.

`UpstreamPackageURL` (`src/repo/upstreamurl.rs:13`) gains an `oci` case
returning the upstream registry's web URL for the name, so an approval reviewer
can open the source image.

## Garbage collection and retention

This is the section that changes existing behavior, and the reason the format
cannot be shipped as "just another handler".

The blob sweeper (`src/repo/sweeper.rs:18`) is already correct: it reclaims
bytes only when `blobs.ref_count` reaches zero and a grace period has passed, and
every OCI blob has an artifact row holding a reference.

Two existing paths are **not** correct for OCI and must be gated off:

- **Idle retention reaper** (`src/repo/reaper.rs:22`) deletes artifacts by
  `last_accessed_at`. A layer blob shared by a rarely-pulled tag is idle by that
  measure while the tag is still live; deleting its row drops the last reference
  and the sweeper then deletes the bytes, leaving a tagged manifest that cannot
  be pulled. The reaper must skip repositories whose format is `oci`.
- **Cache-size eviction** (`Engine.maybeEvict`,
  `src/repo.rs:834`) evicts LRU artifacts against
  `Cache.MaxSizeBytes` and has the same failure mode. It must skip `oci`
  repositories.

Both exclusions must be visible in the UI: when a repository's format is `oci`,
the retention and cache-size controls render disabled with the reason.

Deletion instead goes through a manifest-aware path:

- `DELETE …/manifests/{digest}` removes every `oci_tags` row pointing at the
  digest and the manifest artifact row. `DELETE …/manifests/{tag}` removes only
  the tag row, per spec.
- A new leader-gated `RunOCIPrune`, scheduled alongside the sweeper and reaper
  in `src/bin/forklift.rs`, computes per `(repo_id, name)` the set of digests
  reachable from manifests that are still tagged or still referenced by a
  tagged index, and deletes the blob and manifest artifact rows outside that
  set. The existing sweeper then reclaims the bytes after its grace window. The
  same pass expires stale `oci_upload_sessions` rows and their temp files.
- Reachability is computed from stored manifest bytes, so a partially pushed
  image (blobs uploaded, manifest never `PUT`) is collected exactly like an
  untagged manifest.

## Authentication

`docker login` follows a 401 that carries a `Basic` challenge by sending Basic
credentials on subsequent requests, which is what
`auth.UnauthorizedBasic` (`src/auth/middleware.rs:149`) already emits. No
token service is required in v1:

- Local accounts, OIDC-provisioned accounts and scoped access tokens all work as
  the `docker login` password, which makes a Kubernetes `imagePullSecret` a
  scoped read-only Forklift token.
- Anonymous read follows `authz.AnonymousRead()` exactly as the package formats
  do (`src/repo/router.rs:172`).
- `GET /v2/` must return 401 rather than 404 when unauthenticated, because
  clients treat it as the auth probe.

A `Bearer` token realm (`GET /v2/token`) is a follow-up, needed only by clients
that refuse Basic over plaintext HTTP. Note in the docs that containerd and
CRI-O require HTTPS or an explicit insecure-registry entry; this is a
deployment concern, not a server one.

## Surfaces to update

| Surface | Change |
|---|---|
| `src/meta/models.rs:193` | add `FormatOCI = "oci"`, extend the format comment on `:10` |
| `src/api/repositories.rs:127` | add `meta.FormatOCI` to `validFormats` |
| `src/api/repositories.rs:84` | `PublishMethods` = `["docker", "helm", "oras"]` |
| `src/repo/router.rs:230` | register the `/v2` routes |
| `src/repo/seed.rs:34` | optional seed trio: `oci` proxy of Docker Hub, `oci-hosted`, `oci-public` group |
| `web/src/utils/repository-endpoint.ts` | `oci` returns `{host}/{name}` with the hint `docker login / docker pull` — note it is a host prefix, not a URL |
| `web/src/routes/workspace/repositories/new.tsx` | format option, and hide the upload form for `oci` |
| `web/src/routes/workspace/repositories/$id/-detail.tsx` | tag-oriented artifact rendering; disable retention and cache-size controls |
| `src/openapi` | format enum, regenerate `web/src/services/v1/openapi-types.ts` |
| `docs/usage.md`, `docs/installation.md`, `README.md` | format table, client wiring, HTTPS requirement for containerd |
| `charts/forklift` | nothing structural; `/v2` is served by the existing HTTP port |

The UI upload route is deliberately not extended: `Uploader.Supports`
(`src/repo/uiupload.rs:205`) keeps returning false for `oci`, so
`Capabilities.Upload` is false and no upload affordance appears. Pushing an
image through a browser form is not a workflow worth building; the native
clients are universal.

## Configuration

New environment variables, following `src/config.rs`:

| Variable | Default | Meaning |
|---|---|---|
| `FORKLIFT_OCI_MAX_MANIFEST_BYTES` | `4194304` | cap on a manifest or index document |
| `FORKLIFT_OCI_MAX_BLOB_BYTES` | `0` | per-blob upload cap; 0 is unlimited |
| `FORKLIFT_OCI_UPLOAD_SESSION_TTL` | `24h` | age at which an incomplete push is pruned |
| `FORKLIFT_OCI_PRUNE_INTERVAL` | `1h` | how often `RunOCIPrune` runs on the leader |

Per-repository: `Upload.OCIDockerHubLibraryPrefix` (see [Proxy](#proxy)).

## Metrics

No new metric families. `Engine` already labels ingress and egress bytes by
format (`src/repo.rs`, `bytes` with the `format` label) and cache
hits and misses by repository, which covers the operator question of how much
layer traffic the proxy is absorbing. Two gauges are added because they have no
analogue:

- `forklift_oci_upload_sessions_active`
- `forklift_oci_prune_deleted_total{repo}`

## Implementation slices

Each slice is independently reviewable and leaves the tree shippable. The format
must not appear in `validFormats` until slice 3 is merged, so no user can create
a repository the server cannot fully serve.

1. **Storage and schema.** `FormatOCI`, migration `0030_oci_tags.sql` and
   `0031_oci_upload_sessions.sql`, tag and session store methods with tests. No
   routes.
2. **Pull path.** `/v2/` base, manifest and blob `GET`/`HEAD`, tag list, hosted
   only, digest and tag resolution, OCI error envelope.
3. **Push path.** Upload sessions, monolithic and chunked, manifest verification
   and tag commit, cross-repo mount, `DELETE`. At the end of this slice `docker
   push` and `docker pull` round-trip against a hosted repository, and the
   format is exposed in the API and UI.
4. **Proxy.** Upstream token handshake, Docker Hub library prefixing, cache
   freshness for tags versus digests, 429 passthrough.
5. **Group.** `oci-tags` merge, member-priority manifest resolution.
6. **Garbage collection.** Reaper and eviction exclusions, `RunOCIPrune`,
   session expiry, UI disabling of the excluded controls.

Slice 6 gates general availability. Slices 1 through 5 may merge behind
`validFormats` while GC is still missing only because no data can be lost by a
format nobody can create.

## Test plan

- **Conformance.** The OCI distribution conformance suite
  (`oci-conformance`, workflows `pull`, `push`, `content-management`) runs
  against a hosted repository in CI. `management` cases for referrers are
  expected to fail and are excluded explicitly rather than silently.
- **Round-trip.** `docker push`/`pull`, `helm push`/`pull` for an OCI chart, and
  `oras push` for a plain artifact, asserting byte-identical manifests.
- **Multi-arch.** Push an index with two manifests, pull by platform, confirm
  the index and both children are stored and that deleting one tag leaves the
  other pullable.
- **Session continuity.** A push whose `POST`, `PATCH` and `PUT` are directed at
  three different replicas over a shared volume completes.
- **Verification failures.** `PUT` a manifest referencing an absent blob
  (`MANIFEST_BLOB_UNKNOWN`); finalize an upload with a mismatched digest
  (`DIGEST_INVALID`, nothing stored).
- **GC safety.** With retention `idle_ttl` and a small `max_size_bytes` set on an
  `oci` repository, advance the clock past both and assert every tagged image is
  still pullable and no blob row was deleted. Then untag one image and assert
  `RunOCIPrune` plus the sweeper reclaim exactly its unshared blobs.
- **Policy.** An approval gate on a proxy blocks a first pull of
  `library/nginx:1.27` and admits it after approval; an enabled vuln policy with
  `block_unscanned` does not block an OCI pull.
