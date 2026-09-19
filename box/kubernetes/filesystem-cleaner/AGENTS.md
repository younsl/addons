# AGENTS.md

Guidance for coding agents working in this directory. Only non-derivable conventions and traps live here. Read the code, Makefile, and README for everything else.

## Overview

Filesystem cleaner for Kubernetes, run as an init container (`once` mode) or sidecar (`interval` mode). Rust 1.98.1, edition 2024, statically linked musl binary on `scratch` built with cargo-zigbuild. Keep the dependency set small: the tool is a single binary with no network surface.

## Traps

- **Glob semantics are intentionally non-standard.** `src/matcher.rs` uses globset with its default settings (`literal_separator = false`): `*` and `?` match across `/`, and `**` is special only as a full path component. Do not enable `literal_separator` or swap in a `path.Match`-style matcher, deployed exclude patterns like `*.log` rely on matching at any depth.
- **Patterns match paths relative to the target path**, with forward slashes. Exclude applies to files and directories, include applies to files only.
- **Symlinks are never followed or deleted.** This guards against infinite loops and deletions outside target paths. Keep lstat semantics (`DirEntry::metadata`, never `canonicalize`) when touching the scanner.
- **Disk usage is `(total - available) / total`** from `statvfs(2)`, where available is the unprivileged (`f_bavail`) figure. This can differ from `df`.
- **`tracing` skips field expressions when no subscriber is installed.** Tests install none, so a method only called inside `info!(field = value.method())` never runs under test and shows 0% coverage. Test such helpers directly.
- **Config resolution bypasses clap's `env` feature on purpose.** `Config::resolve` takes an env lookup closure so flag > env > default precedence is unit-testable without `std::env::set_var`, which is `unsafe` in edition 2024 and blocked by `unsafe_code = "forbid"`. Add new settings through `Args` plus `Config::resolve`, not `#[arg(env = ...)]`.

## Conventions

- Filesystem access sits behind the `DiskUsage` (`src/disk.rs`) and `FileRemover` (`src/remover.rs`) traits. Test cleanup policy through `Cleaner::with_backends` with fixed usage and a recording remover, never by relying on the real disk being above or below a threshold. Scheduling lives in `src/schedule.rs`, cycle results in `src/cleaner/report.rs`.
- The image runs as `USER 65532:65532` on `scratch` with no CA certificates: the cleaner makes no network calls, so do not add TLS-dependent features without updating the Dockerfile.
- In Kubernetes the cleaner must run as the same UID/GID as the container whose files it deletes (e.g. `1001` for actions-runner) and share the same volume mount path.
- CI gates on `cargo fmt --check`, `cargo clippy -- -D warnings` (pedantic and nursery are enabled as warnings in Cargo.toml, so they fail CI), tests in debug and release, and 70% line coverage via `cargo llvm-cov`. Release triggers are documented in the repo root AGENTS.md.
