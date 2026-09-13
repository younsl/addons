# Rust migration release validation

Validation date: 2026-09-08. These results describe the local working tree;
no image, chart, tag, or release has been published by this validation.

## Dependencies and toolchain

- Both binaries use Rust 1.98.1 and the same Cargo dependency graph.
- All 79 distinct direct and development crates match the current crates.io
  stable versions. Requirements are recorded in `Cargo.toml`; `Cargo.lock`
  records the resolved graph. This includes the direct `syn` 3 upgrade.
- `cargo update --dry-run` reports zero updates for the compatible graph.
- Three transitive dependencies cannot all move forward in that graph:
  `matchit` 0.8.4 is pinned exactly by `axum`; `smallvec` 1.15.2 is constrained
  below 1.16 by `serde-saphyr`; selecting `crypto-common` 0.1.7 requires
  downgrading `generic-array` from 0.14.9 to 0.14.7. No forced overrides were added.
- Container builders bootstrap from the available Rust 1.98.0 image and
  explicitly install **1.98.1** through rustup. The unique
  `FORKLIFT_RUST_VERSION` argument avoids the base image's inherited
  `RUST_VERSION` environment variable. Builds pin cargo-zigbuild 0.23.4 and
  Zig 0.15.1, and runtime stages use scratch with UID/GID 65532.

## Completed checks

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| Rust unit/integration tests | 709 passed, zero ignored |
| Rust documentation example | Compiled successfully, zero ignored |
| `cargo llvm-cov --all-targets --locked` | 88.1318% line coverage (57,016 / 64,694) |
| Web production build and TypeScript | Passed |
| Web Vitest | 187 tests in 27 files passed |
| E2E TypeScript | Passed |
| Chromium E2E, two workers, no retries | 88 passed, zero skipped |
| Chromium E2E against the final arm64 scratch image | 88 passed, zero skipped |
| OCI distribution-spec v1.1.1 | Pull, push, discovery, management suite passed |
| Introduction site production build | Passed |
| GitHub Actions `actionlint` | Passed |
| Both binaries, musl amd64 + arm64 | Statically linked, stripped release ELFs |
| scratch runtime, amd64 + arm64 | Forklift readiness/UI and MCP readiness/metrics/initialize passed |
| Both actual Dockerfiles, arm64 | Full builds passed; runtimes report Rust 1.98.1 |
| `git diff --check` | Passed |

The E2E suite now starts a Rust server against a fresh isolated database,
loads a declarative RBAC fixture, and rejects unexpected browser 404s. The old
POST-on-404 retry workaround is removed. Test pages close before fixture
resources are deleted, preventing cleanup from racing active page queries.
Bulk-label tests also wait for selection reset and control re-enablement before
starting the next operation, rather than treating the HTTP response as UI completion.

Six format scenarios each upload two versions through the UI, compare native
protocol downloads byte-for-byte, and add/remove a label using multiple checked
artifact rows. Raw bulk deletion is driven through the UI. Managed lifecycle
API checks verify Maven, npm, and PyPI deletion; Cargo yank retains downloadable
bytes; Go publication deletion is refused with 409 and retains bytes. These
last two behaviors preserve the existing ecosystem immutability policy.

An additional OCI scenario pushes and downloads two configurations/manifests,
checks native manifest deletion, and exercises multi-selection label changes
in the browser. OCI previously offered only individual label editing; the new
controls use the existing bulk API, deduplicate shared manifest paths, preserve
failed selections for retry, and disable unauthorized rows. The E2E also checks
that a read-only user cannot select or bulk-label these images.

A cancellation regression discovered during E2E was fixed in the SQLite read
pool: an RAII lease returns a connection even when its async caller is aborted
or its blocking task panics. A pinned change watcher uses the same ownership
rule. Three regression tests exercise these failure paths.

## Implementation cleanup

The application Go sources and root Go module files are removed. The obsolete
`bin/forklift` executable was identified as Go 1.27.0 and deleted. Build,
Playwright, generated-client provenance, and product documentation now refer
to the Rust implementation. Go module repository support and its package
fixtures remain part of the product; the official OCI conformance suite is an
external test tool, not the Forklift implementation.

## Security scan result

The eight open [Dependabot alerts](https://github.com/younsl/addons/security/dependabot)
queried on 2026-09-08 were all npm development dependencies in
`web/pnpm-lock.yaml`, not Go dependencies. Local `pnpm --dir web audit`
reproduced the same eight GHSA IDs (six high, two moderate).

| Package | Alerts | Previous | Updated |
| --- | --- | --- | --- |
| fast-uri | 55–58 | 3.1.5 | 3.1.6 |
| qs | 53–54 | 6.15.2 | 6.16.0 |
| browserslist | 51–52 | 4.28.2 | 4.28.9 |

The updated lockfile satisfies the patched-version requirements of all eight
GitHub alerts. Both web and site pnpm audits report zero vulnerabilities,
including development dependencies. The existing fast-uri override now permits
the security patch; qs and browserslist were updated within their parent ranges.
CI and the Forklift container release test job now run `pnpm --dir web audit`
without ignored advisories or registry errors. Remote alerts remain open until
the changes reach the default branch and Dependabot re-evaluates them.

After these dependency updates, frozen-lockfile installation, the web build,
all 187 web unit tests, all 88 Chromium E2E tests (two workers, no retries),
actionlint, and whitespace checks passed. This follow-up E2E used the local
Rust server and Vite. The scratch-image validation recorded above preceded
these dependency updates; those images have not been rebuilt in this follow-up.

Scanner coverage differs: a local Trivy filesystem scan reported zero findings
even before these fixes, while pnpm reproduced all eight GitHub alerts and
cargo-audit reported the Rust advisory below. Trivy's zero count is therefore
not evidence that the dependency tree is clean.

`cargo audit` exits with a finding for
[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html):
`openidconnect` 4.0.1 depends on `rsa` 0.9.10, and the advisory currently has no
patched version. The advisory concerns RSA private-key timing leakage.
Forklift's OIDC implementation verifies ID tokens with public keys and uses
OAuth client credentials; it does not perform RSA private-key operations.
This usage assessment does not turn the scanner result into a passing check.
No advisory was suppressed and no authentication dependency was replaced merely
to obtain a clean scan.

## Remaining release work

Both actual Dockerfiles completed full arm64 builds. The final UI is embedded
in the refreshed Forklift release artifacts and was tested directly from the
read-only scratch image as UID/GID 65532. Local amd64/arm64 musl builds also
completed. Zig emitted a non-fatal deprecated linker optimization warning.

Release version metadata is set: Forklift 0.13.0 (`Dockerfile` label), MCP
0.3.0 (`Dockerfile.mcp` label), chart 0.12.0 with appVersion 0.13.0 and MCP
tag 0.3.0 (`charts/forklift`). None of those tags existed on GHCR when checked
on 2026-09-08, so the release workflows publish all three once the change
reaches `main`. The security scan finding above remains visible in the release
assessment.

## Final re-verification

A last pass over the same working tree on 2026-09-08, after the version bump,
repeated the checks: `cargo fmt`, `cargo clippy -D warnings`, 709 Rust tests,
88.13% line coverage, web build with 187 Vitest tests, 88 Chromium E2E tests,
`helm lint` plus rendering with `mcp.enabled`, `actionlint`, and `git diff
--check` all passed. The release binary built with `--locked` was started from
a fresh data directory and served health, readiness, the embedded UI, the
management API, every proxy format (npm, Maven, Cargo, Go, PyPI, OCI), a raw
hosted upload, and an OCI blob push. The HTTP request metrics keep the Go
label encoding (`status` carries the reason phrase, e.g. `OK`), so existing
dashboards do not change. The stale Go 1.27 `forklift` executable and
`cover.out` in the repository root, both gitignored, were deleted.

## Post-validation review fixes (2026-09-09)

An independent review of the working tree compared the Rust routes,
configuration, metrics, SQLite compatibility and MCP surface against the Go
implementation at the previous commit. It found and fixed the following:

- Format routes with an empty tail (`POST /pypi/{repo}/`, `GET /maven/{repo}/`)
  fell through to the console fallback and answered 200 with `index.html`,
  because an axum `{*rest}` wildcard does not match an empty segment while the
  chi `/*` pattern did. twine is commonly configured with a trailing slash on
  the repository URL. Explicit `/{format}/{repo}/` routes restore the previous
  behaviour: the PyPI root accepts uploads, the other formats answer 400.
- `forklift_http_requests_total` and `forklift_http_request_duration_seconds`
  spelled the `route` label as `/maven/{repo}/{*rest}`; the label is
  normalised back to `/maven/{repo}/*` so existing dashboards keep matching.
- Persisted JSON written by the Go release stores empty lists as `null`
  (`coverage_results.payload`, `artifact_upload_requests.result_json`). The
  Rust structs rejected `null` for a list, which dropped the last coverage
  result until the next scan and returned 503 on an idempotent upload replay.
  Those fields now decode `null` as an empty list.
- The `run_starts_and_shuts_down` test bound the default profiling address
  and used a misspelled `FORKLIFT_DEPS_DEV_URL`; it now isolates the pprof
  listener and disables deps.dev resolution.

Known, unchanged differences with the Go release: `process_*` collectors are
Linux-only and `go_*` series are gone; the OCI upload-session gauge is
refreshed asynchronously and reports -1 on the first scrape; `:port` listen
addresses bind IPv4 `0.0.0.0` rather than dual-stack; the MCP server advertises
protocol version 2025-11-25 and maps invalid tool arguments to JSON-RPC
`-32602` rather than an `isError` result; `/api/v1/approvals/` with a trailing
slash is no longer routed.
