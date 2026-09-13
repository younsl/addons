# Development

## Overview

This document covers working on forklift itself: the make targets, how the
frontend is built into the binary, where design documents live, and how a
release is cut.

Read this before the first local build or the first pull request.

## Background

The repository holds one Rust crate and one frontend. The binary embeds the
built [React](https://github.com/facebook/react) UI at compile time
(`rust-embed`), so a release is a single artifact with no separate static file
server. A placeholder
`dist` is committed, which is why `make build` succeeds on a clean checkout
without ever running the frontend toolchain; `make web-build` replaces it with
the real UI, and the release workflow runs the same build before compiling.

Releases are label-driven rather than tag-driven. The container image version
comes from an OCI label in the `Dockerfile` and the chart version from
`Chart.yaml`, so changing either on `main` is what publishes a new version.

```bash
make build        # release binaries (forklift, forklift-mcp) into bin/
make cross        # static musl binaries for linux/amd64 and linux/arm64 via cargo-zigbuild
make test         # cargo test
make lint         # rustfmt check + clippy with warnings denied
make coverage     # enforce 73% line coverage (cargo-llvm-cov)
make web-build    # build the React UI into src/webui/dist
make dev          # run with debug logging and a local ./.data dir
make docker-build # multi-arch scratch image (builds the UI and the musl binaries first)
make helm-lint    # lint the chart
```

## Toolchain

The crate pins Rust 1.98.1 in `rust-toolchain.toml`; `rustup` picks it up on
first build. The container image and `make cross` link the Linux binaries with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild), which drives a
`zig cc` cross toolchain for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, so both architectures build on one machine
without QEMU. Install it with `pip install cargo-zigbuild` (ships zig) or
`cargo install cargo-zigbuild` plus a zig on `PATH`. The bundled SQLite and the
`ring` crypto backend are the only C code in the build.

The Docker builders start from the published `rust:1.98.0-slim-bookworm`
image and install Rust 1.98.1 with rustup. This keeps the compiler pinned even
while the official Docker images lag the Rust patch release.
Both builders pin cargo-zigbuild 0.23.4 and Zig 0.15.1, and both release
workflows pass their Rust version as `FORKLIFT_RUST_VERSION` to avoid the
base image's inherited `RUST_VERSION` environment variable.

Browser tests also start the Rust binary through `cargo run --locked`. Install
the matching browser once with `pnpm --dir web exec playwright install chromium`,
then run `make e2e`. The backend uses port 8090, the UI uses 5273, and test data
lives in `.e2e-data`; the metrics and profiling listeners each get a free port.

The React UI lives in `web/` ([Vite](https://github.com/vitejs/vite) + TypeScript) and is embedded into the binary with `rust-embed`. A placeholder is committed so `make build` works without a frontend build; `make web-build` or the Docker build produces the real UI.

## Design documents

Design documents live in [designs/](designs/) and each one opens with an explicit implementation status. Web UI changes follow [designs/linear-inspired.md](designs/linear-inspired.md) and [designs/tokens.md](designs/tokens.md); changing a colour token's value also follows [designs/color-contrast-ramp.md](designs/color-contrast-ramp.md).

## Releases

The container image is released from the `org.opencontainers.image.version` label in the `Dockerfile` (the MCP image from `Dockerfile.mcp`), and the chart from the version in `charts/forklift/Chart.yaml`; pushing a change to either on `main` triggers the matching workflow in the repository's `.github/workflows/`. The images are built by `_release-rust-scratch-containers.yml`, which cross-compiles the binaries on the runner: both Dockerfiles are `scratch` images that only copy the finished binary in.
