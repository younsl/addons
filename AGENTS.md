# AGENTS.md

Guidance for coding agents working in this repository.

This file records only what the repository cannot tell you by itself: conventions, policies, and traps.
Anything derivable by reading the code is deliberately omitted. Do not re-add directory trees, Makefile
target lists, or per-tool feature summaries here; read the code or the tool's own README instead.

## Overview

Monorepo of Kubernetes addons, operators, CLI tools, runtime container images, and a Zola blog.
Most applications are Rust. The exception is `backstage` (Node.js/React).

Each addon follows the Unix philosophy of doing one thing well. Prefer a new small component over
extending an existing one past its purpose.

## Commit Messages

Format: `[<TOOLNAME>] <type>(<scope>): <detail message>`

- `<TOOLNAME>` is the component directory name. Use `[repo]` for changes that span components.
- Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`.

Examples:
- `[ij] refactor(scan): improve multi-region parallel scanning`
- `[repo] chore(docs): update AGENTS.md`

## Generated Files

Chart READMEs come from helm-docs (`box/kubernetes/charts/README.md.gotmpl`, regenerate with `make -C box/kubernetes/charts docs`); never edit one directly, a pre-commit hook reverts it.

## Release Triggers

Every release triggers on merge to `main` when a version value inside a file changes, and skips if that
version was already published. No release is tag-based.

| Artifact | Bump this | Effect |
| --- | --- | --- |
| Container image | `org.opencontainers.image.version` label in the Dockerfile | Builds and pushes to GHCR |
| Helm chart | `version` in `Chart.yaml` | Pushes to `ghcr.io/younsl/charts/{chart}` |
| Rust CLI (`ij`) | `version` in `Cargo.toml` | Builds release binaries and cuts the `ij/x.y.z` release |

Release decisions (what changed, what is already published) live in `.github/scripts/ci-*.sh`; the
workflows only wire environment and matrices into them. The Rust scratch-container projects are listed
in `.github/scripts/ci-rust-projects.json`.

Which workflow owns a given container is decided by the `paths:` list at the top of each workflow in
`.github/workflows/`. Check there rather than trusting any list written elsewhere. Editing a Dockerfile
without touching its version label triggers a workflow run that then no-ops, which is the intended way
to make non-releasing changes.

To rebuild a version already published, dispatch the workflow with `force=true`, which overwrites the
tag. Deleting the registry artifact first is not an alternative: ghcr refuses to delete the last
tagged version of a package, and deleting the package instead drops `latest` and resets the visibility
that downstream proxy caches pull through.

Charts are distributed only as OCI artifacts. Use [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md)
to discover versions (`crane ls ghcr.io/younsl/charts/{chart}`); `helm search repo` does not work.

## Testing

Applications under `box/kubernetes/` and `box/tools/` must hold at least 70% line coverage, measured
with `cargo llvm-cov`. Check before releasing, not after.

## Rust Conventions

- 2018+ module style: `foo.rs` alongside a `foo/` directory. Never `foo/mod.rs`.
- The root `.gitignore` ignores every path named `config` (AWS credentials). `**/src/config` is
  whitelisted there; a `config/` directory anywhere else needs its own exception or its files silently
  stay untracked and CI fails on a missing module.
- Unit tests in a `#[cfg(test)]` module in the same file; integration tests in `tests/`.
- Container images are `scratch` with statically linked binaries built via cargo-zigbuild.
- Cross-compilation needs a target C toolchain and linker configuration. The working setup
  lives in `.github/workflows/_release-rust-scratch-containers.yml`.

## Documentation

Write all repository documentation in concise English, including READMEs and this file.
