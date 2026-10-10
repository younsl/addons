# AGENTS.md

Guidance for coding agents working in this repository.

This file records only what the repository cannot tell you by itself: conventions, policies, and traps. Do not re-add directory trees, Makefile target lists, or per-tool feature summaries. Read the code or the tool's own README instead.

## Overview

Monorepo of Kubernetes addons, operators, CLI tools, and runtime container images, plus personal docs under `docs/`. Components with an image live in `box/addons/`, standalone Helm charts in `box/charts/`, CLIs in `box/cli/`. Every component is built in Rust except `backstage` (Node.js/React). `trivy-collector` and `forklift` embed a web frontend that the image build compiles first.

Each component does one thing well. Prefer a new small component over extending an existing one past its purpose.

### Public repository

Everything committed here is public, including commit messages and PR attachments. Never commit company names, internal domains, hostnames, IP ranges, account IDs, ARNs, credentials, personal info, ticket keys (Jira issue IDs), internal service names, or screenshots taken against company systems. Use `example.com`, `10.0.0.0/16`, `123456789012`, `${SECRET_NAME}` instead.

## Git

Commit directly to `main` with a DCO sign-off (`git commit -s`). Do not open a branch for routine work.

Several agent sessions share this working tree and git index, so the index often holds files you did not stage. Never run `git add -A` or `git add .`. Commit with a pathspec (`git commit -s -- box/addons/<component>`) so other sessions' staged files stay out, then check the scope with `git show --name-only HEAD`.

### Commit messages

Format: `[<TOOLNAME>] <type>(<scope>): <detail message>`

- `<TOOLNAME>` is the component directory name. Use `[repo]` for changes that span components and `[kb]` for knowledge base notes.
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`.
- No `Claude-Session` trailer in commit messages or PR bodies.

Examples: `[ij] refactor(scan): improve multi-region parallel scanning`, `[repo] chore(docs): update AGENTS.md`.

## Writing

Applies to READMEs, docs, this file, commit messages, and chart annotations.

- Concise English.
- No em dash (`—`), middle dot (`·`), or semicolon. Use a period, comma, colon, or parentheses. Code and command snippets are exempt.
- One physical line per paragraph or list item. Hard wraps turn a one-word edit into a multi-line diff.
- README prose paragraphs (announcements, introductions) prefer plain text over inline code. Fenced command blocks and tables are fine.

## Rust

Every crate is standalone (no workspace). Match these when adding or touching one.

### Crate

- `edition = "2024"` in every crate, new ones included.
- `rust-version` equals the repository toolchain (`1.99.0`). Bump it in every crate together.
- `[lints.rust] unsafe_code = "forbid"` and `[lints.clippy]` with `all`, `pedantic`, and `nursery` at `warn`. A lint that fires across the crate is allowed in `Cargo.toml` with a one-line reason, which requires the groups as `{ level = "warn", priority = -1 }` or clippy fails on `lint_groups_priority`. A single site uses an item-level `#[allow]`.
- `[profile.release]` sets `lto = true`, `codegen-units = 1`, `strip = true`, and `panic = "abort"` for long-running services. `opt-level = "z"` for small I/O-bound services, `3` otherwise.
- Only the tokio features the crate uses, never `full`.
- 2018+ module style: `foo.rs` alongside a `foo/` directory, never `foo/mod.rs`.

### Runtime

- Kubernetes operators and controllers use [kube.rs](https://github.com/kube-rs/kube): `kube::runtime::Controller` for reconcile loops and `#[derive(CustomResource)]` for CRDs. No other client library, no shelling out to `kubectl`.
- TLS is rustls only. Never pull in openssl or native-tls, which breaks the static `scratch` build. Set `default-features = false` on `reqwest` and `kube` and enable the rustls features explicitly.
- Install the `aws-lc-rs` crypto provider once at the top of `main` so every rustls client and listener shares it.
- Errors: `thiserror` enums inside modules, `anyhow` only at the binary boundary (`main` and CLI glue).
- Configuration comes from `clap` derive with an `env` fallback on every flag, so Helm sets values through environment variables.
- Logging uses `tracing` with JSON output by default. Expose `--log-level`/`LOG_LEVEL` and `--log-format`/`LOG_FORMAT` (`json` or `text`), and let `RUST_LOG` override the level through `EnvFilter`.
- Long-running services serve `/healthz`, `/readyz`, and `/metrics` and shut down gracefully on SIGTERM. Use `prometheus-client` for new metrics code.
- A service that publishes an OpenAPI document (utoipa) registers every endpoint in it: `#[utoipa::path]` on the handler and an entry in `ApiDoc`. An endpoint missing from the document is unfinished.

### Verification

- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test` pass after every change. The pre-commit hook runs them for changed crates. Run `cargo fmt` after the last edit, tests included.
- Unit tests in a `#[cfg(test)]` module in the same file. Integration tests in `tests/`.
- Every Rust application under `box/` holds at least 70% line coverage, measured with `cargo llvm-cov`. The release workflows gate on it, so check before any release, force re-releases included.

### CodeQL

CodeQL scans test code too and reports it as a real alert.

- Every axum handler argument counts as user input, `State` included. A file path read from config and reached through a handler is flagged `rust/path-injection`. Remove the file system access from the request path. String checks such as filtering `..` are not barriers.
- Printing a value that holds a credential is flagged `rust/cleartext-logging`, even inside a `#[cfg(test)]` module. Assert on it instead of printing it.

### Build

- Container images are `scratch` with statically linked binaries built via cargo-zigbuild. The working cross-compilation setup (toolchain, linker, pinned zig version) lives in `.github/workflows/_release-rust-scratch-containers.yml`.
- The root `.gitignore` ignores every path named `config` (AWS credentials). Only `**/src/config` and `**/internal/config` are whitelisted. A `config/` directory anywhere else needs its own exception, or its files silently stay untracked and CI fails on a missing module.

## Helm Charts

- Every chart ships a `values.schema.json` (draft-07), updated in the same commit as `values.yaml`, so misspelled keys and wrong types fail `helm lint` and `helm template` instead of being silently ignored.
- Root and chart-owned blocks set `additionalProperties: false` and the root allows `global`. App config blocks mirror the Rust config structs. Kubernetes pass-through fields (`resources`, `affinity`, `securityContext`, ...) are type-only.
- Every key in `values.yaml` carries a helm-docs comment in the form `# -- (type) description`. Commented-out examples stay plain `#`.
- Reference blocks in templates and examples (`parentRefs`, `backendRefs`, and similar) set every identifying field (`group`, `kind`, `name`, `namespace`). Relying on spec defaults hides the target, and a missing `namespace` points at the wrong one.
- Charts are distributed only as OCI artifacts. Discover versions with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md) (`crane ls ghcr.io/younsl/charts/{chart}`). `helm search repo` does not work.

## Web UI

Embedded frontends (`trivy-collector`, `forklift`) never use browser-native `alert()`, `confirm()`, `prompt()`, or `title` tooltips. Native dialogs and tooltips cannot be styled, look different in every browser, and break the app's look and feel. `alert()`, `confirm()`, and `prompt()` also block the page until dismissed. Use the app's own modal and tooltip components.

## Releases

Every release triggers on push to `main` when a version value inside a file changes, and skips if that version was already published. No release is tag-based.

| Artifact | Bump this | Effect |
| --- | --- | --- |
| Container image | `org.opencontainers.image.version` label in the Dockerfile | Builds and pushes to GHCR |
| Helm chart | `version` in `Chart.yaml` | Pushes to `ghcr.io/younsl/charts/{chart}` |
| Rust CLI (`ij`) | `version` in `Cargo.toml` | Builds release binaries and cuts the `ij/x.y.z` release |
| Harbor arm64 images | `box/addons/harbor/VERSION` | Rebuilds upstream Harbor images for arm64 |

- Release decisions (what changed, what is already published) live in `.github/scripts/ci-*.sh`. Workflows only wire environment and matrices into them. Rust scratch-container projects are listed in `.github/scripts/ci-rust-projects.json`.
- The `paths:` list at the top of each workflow in `.github/workflows/` decides which workflow owns a component. Trust it over any list written elsewhere.
- Editing a Dockerfile without touching its version label runs the workflow and then no-ops. This is the intended way to make non-releasing changes.
- A re-release keeps the same version: fix, push, and dispatch the workflow with `force=true`, which overwrites the tag. Bump a version only when asked.
- Never delete the registry artifact to make room for a rebuild. ghcr refuses to delete the last tagged version of a package, and deleting the whole package drops `latest` and resets the visibility that downstream proxy caches pull through.

## Generated Files

Never edit these by hand. A pre-commit hook regenerates them.

| File | Source | Regenerate |
| --- | --- | --- |
| Chart `README.md` | helm-docs with `box/charts/README.md.gotmpl` or a chart-local `README.md.gotmpl` | `make -C box/charts docs` |

## Knowledge Base

`box/kb/` holds personal SRE notes, one directory per domain (`kubernetes/`, `observability/`, ...) and one subject per kebab-case file. Commit with `[kb] docs(<domain>): ...`. lychee checks every link in `box/kb/`, and `scripts/kb-lint.sh` validates the frontmatter and sections below.

Notes come from real work. Incident write-ups describe the failure mode and fix, not the affected internal service.

Every note starts with YAML frontmatter so agents can find relevant notes without reading bodies:

- `description`: one sentence stating the rule and its main reason
- `tags`: lowercase keywords, domain first
- `resources`: Kubernetes kinds or tool objects the rule applies to, omitted when none
- `status`: `adopted`, or `superseded` with `superseded_by` naming the replacing note file. Never apply a superseded note
- `reviewed`: date the content was last verified against current upstream docs (`YYYY-MM-DD`). Bump it only after re-checking, not on every edit

The body is one H1 followed by these H2 sections in order: `Rule`, `Why`, `Exceptions` (optional), `Example` (optional), `References`. Use H3 for anything else inside them.

Link every major keyword (tool, Kubernetes object or field, AWS service, protocol concept) to its official documentation or GitHub repository at its first mention in the body. Frontmatter, headings, and code blocks stay unlinked. Prefer a deep link with an anchor over a landing page.

Add, rename, or supersede a note together with its row in `box/kb/README.md`, which is written by hand.

Find notes for a kind with `rg -l 'resources:.*\bService\b' box/kb`, or scan `box/kb/README.md` for one-line summaries.
