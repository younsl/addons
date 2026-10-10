# Security policies

## Overview

This document covers the supply-chain gates a proxy repository can apply before
it serves a package: package approval, exact-version denies, vulnerability
policy, and license policy. Each section gives the API call, the behavior on a
block, and the metrics and audit events it produces.

Read this when hardening a proxy against malicious or unwanted packages, or when
calibrating a policy before enforcing it.

## Background

A proxy repository fetches from a public registry, which makes it the natural
place to stop a bad package before it reaches a build. Commercial repository
managers such as [Nexus Repository](https://github.com/sonatype/nexus-public)
call this a firewall or quarantine; forklift models the same idea as a small set
of independent gates, each configured per repository.

The gates differ in what they decide and when. Approval is a human decision
about a whole package. A version deny is an incident-response switch for one
exact version. Vulnerability and license policies are automated assessments
whose data is gathered out of the serving path, so a request consults a stored
verdict instead of waiting on an external API. The age policy, configured
alongside them, gates versions by how recently they were published.

Each gate has an enforcing mode and a recording mode. Running in `audit` first
shows what enforcement would have blocked, which is the safe way to introduce a
policy to an active build fleet.

Proxy repositories can gate what they serve at four independent points: package approval, version denies, vulnerability policy, and license policy. Age policy (quarantining freshly published versions) is configured per repository in the UI or REST API.

Who can change these: administrators, plus roles carrying the `security` action; `approve` decides individual packages. See [Access control](access-control.md).

## Package approval (quarantine)

Require an explicit decision before any package is served, modeled after [Nexus Firewall](https://github.com/sonatype/nexus-public)'s quarantine:

```bash
curl -u admin:change-me -X PUT http://forklift/api/v1/repositories/<id> \
  -H 'Content-Type: application/json' \
  -d '{"upstream_url":"https://registry.npmjs.org",
       "config":{"approval":{"enabled":true,"mode":"enforce","auto_approve":["@company/*"]}}}'
```

- Requests for unapproved packages return 403 (`package pending approval: <pkg>`) and enqueue a pending request; the package never reaches upstream until approved. Approve/reject from the Approvals page in the UI or via `POST /api/v1/approvals/{id}/approve|reject`.
- The decision unit is the whole package (npm package, PyPI project, `group:artifact`, crate, Go module path). Version freshness stays with the age policy: approval admits the package, the age policy still gates versions inside it.
- Rejecting a package blocks it immediately, including content already in the cache.
- `mode: "audit"` serves traffic normally but records demand and counts what enforce would have blocked (`forklift_approval_blocked_total{mode="audit"}`), which is the recommended way to calibrate before enforcing.
- `auto_approve` glob patterns (e.g. an internal npm scope) bypass approval entirely.
- In a group, a blocked gated member is authoritative: the group returns 403 instead of falling through to the next member.
- Pending queue size is exported as `forklift_approval_pending`; decisions land in the repository audit log as `approval.request/approve/reject` events.
- Who can decide: administrators, plus any role carrying the `approve` action, scoped by repo pattern like every other permission. This lets a security team approve packages without repository management rights, e.g. `{"repo_pattern":"npm-*","actions":["read","approve"]}`. Personal access tokens can never approve: token scopes accept only read/write/delete/audit, so a leaked CI token cannot taint approval decisions.

## Version denies

For incident response, one exact version can be cut off while the package stays trusted (a poisoned release, an IOC match):

```bash
curl -u admin:change-me -X POST http://forklift/api/v1/version-denies \
  -H 'Content-Type: application/json' \
  -d '{"repo":"npm-proxy","package":"lodash","version":"4.17.99","reason":"CVE-2026-0001"}'
```

- A deny is an explicit security decision, so it always enforces: it works on any proxy repository regardless of the approval policy, ignores audit mode, and overrides package-level approval.
- The deny runs before any cache lookup, so already-cached copies stop being served immediately. Requests for the version return 403 (`version denied: <pkg>@<ver>`); other versions keep flowing.
- The version is matched exactly as it appears in request paths (go modules keep the `v` prefix). Metadata still lists denied versions; the artifact fetch fails loudly, which for a poisoned release beats the resolver silently picking another version.
- Manage from the Version denies section on the Approvals page (or the repository's Approvals tab), or via `GET/POST /api/v1/version-denies` and `DELETE /api/v1/version-denies/{id}`. The same `approve` permission applies, scoped per repository.
- Blocks are counted in `forklift_version_deny_blocked_total{repo}`; changes and blocked attempts land in the repository audit log as `deny.create/delete/block` events.
- Deny entries are deleted with their repository, so a recreated same-name repo does not inherit them.

## Vulnerability policy (OSV)

Gate package versions by known vulnerabilities, matched against the [OSV](https://github.com/google/osv.dev) database:

```bash
curl -u admin:change-me -X PUT http://forklift/api/v1/repositories/<id> \
  -H 'Content-Type: application/json' \
  -d '{"upstream_url":"https://registry.npmjs.org",
       "config":{"vuln":{"enabled":true,"threshold":"high","action":"block","ignore":["CVE-2026-0001"]}}}'
```

- Scanning is decoupled from enforcement: whenever a scanner is configured (`--osv-url` set), every cached or uploaded coordinate (ecosystem, package, version) is scanned against OSV, even on repositories with no policy enabled, so vulnerability data is always populated. The per-repository policy only governs blocking/warning. Scanning runs out of the serving path: a coordinate is enqueued the moment it is cached/uploaded and a pool of workers drains the queue, so results appear promptly; the request-time gate consults the stored verdict. A periodic re-scanner refreshes results so newly disclosed advisories on already-cached versions surface.
- `action`: `block` (403, refuse to serve), `warn` or `audit` (serve, record). The gate triggers when the highest non-ignored advisory severity meets `threshold` (`critical`/`high`/`medium`/`low`, default `high`). `ignore` lists accepted/false-positive advisory ids (CVE/GHSA/OSV).
- A not-yet-scanned version is served while its scan is queued, unless `block_unscanned` is set under an enforcing (`block`) posture.
- Scope (v1): direct dependency coordinate match only. Transitive dependencies, artifact integrity/provenance, and dependency-confusion are out of scope. A clean result means "no matching public OSV advisory", not a guarantee.
- Configure via flags: `--osv-url` (default `https://api.osv.dev`; empty disables scanning), `--vuln-rescan-interval` (default `6h`), `--vuln-ttl` (default `24h`), `--vuln-workers` (default `6`). The matching `FORKLIFT_OSV_URL` / `FORKLIFT_VULN_RESCAN_INTERVAL` / `FORKLIFT_VULN_TTL` / `FORKLIFT_VULN_WORKERS` env vars still seed the defaults; flags take precedence.
- Blocks are counted in `forklift_vuln_blocked_total{repo,action}` and scans in `forklift_vuln_scans_total{result}`; blocks land in the audit log as `vuln.block` events. Per-version severity shows in the repository's Artifacts tab.

## License policy (deps.dev)

Gate package versions by their declared SPDX license, resolved from [deps.dev](https://github.com/google/deps.dev):

```bash
curl -u admin:change-me -X PUT http://forklift/api/v1/repositories/<id> \
  -H 'Content-Type: application/json' \
  -d '{"upstream_url":"https://registry.npmjs.org",
       "config":{"license":{"enabled":true,"action":"block","deny":["GPL-3.0","AGPL-3.0"],"allow":["MIT","Apache-2.0"]}}}'
```

- Resolution is decoupled from enforcement: whenever a resolver is configured (`--deps-dev-url` set), every cached or uploaded coordinate (system, package, version) is resolved against deps.dev, even on repositories with no policy enabled, so license data is always populated. The policy only governs blocking/warning. Resolution runs out of the serving path: a coordinate is enqueued the moment it is cached/uploaded and a pool of workers drains the queue, so results appear promptly; the request-time gate consults the stored result. A backfill resolves already-stored artifacts and a periodic re-resolver refreshes stale results.
- `action`: `block` (403, refuse to serve), `warn` or `audit` (serve, record). A version carrying any license in `deny` is gated; when `allow` is non-empty, a version carrying any license outside it is also gated (allow-list mode). Matching is case-insensitive.
- A not-yet-resolved version is served while its resolution is queued, unless `block_unresolved` is set under an enforcing (`block`) posture. Applies to proxy repositories; hosted uploads are resolved for display only.
- Scope: direct dependency coordinate match only; the SPDX value is whatever deps.dev reports. Transitive dependencies and artifact integrity are out of scope.
- Configure via flags: `--deps-dev-url` (default `https://api.deps.dev`; empty disables resolution), `--license-rescan-interval` (default `24h`), `--license-ttl` (default `7d` / `168h`), `--license-workers` (default `6`). The matching `FORKLIFT_DEPSDEV_URL` / `FORKLIFT_LICENSE_RESCAN_INTERVAL` / `FORKLIFT_LICENSE_TTL` / `FORKLIFT_LICENSE_WORKERS` env vars still seed the defaults; flags take precedence.
- Blocks are counted in `forklift_license_blocked_total{repo,action}` and resolutions in `forklift_license_resolves_total{result}`; blocks land in the audit log as `license.block` events. Per-version licenses show in the repository's Artifacts tab.

The full license workflow, including the UI surfaces, is in [license-scanning.md](license-scanning.md).

## Related documents

- [Access control](access-control.md) for the `approve`, `audit` and `security` actions.
- [Metrics](metrics.md) for the gate counters above.
- [designs/policy-pipeline.md](designs/policy-pipeline.md) for the evaluation order these gates run in.
