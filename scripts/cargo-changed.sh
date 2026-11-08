#!/usr/bin/env bash
#
# Run a cargo check in every crate that owns one of the given files.
# Called by pre-commit with the changed files as arguments.
#
#   ./scripts/cargo-changed.sh fmt    box/cli/ij/src/main.rs
#   ./scripts/cargo-changed.sh clippy box/cli/ij/src/main.rs
#   ./scripts/cargo-changed.sh test   box/cli/ij/src/main.rs

set -euo pipefail

cmd="${1:?usage: $0 <fmt|clippy|test> <file>...}"
shift

case "$cmd" in
  fmt | clippy | test) ;;
  *) echo "unknown command: $cmd" >&2; exit 2 ;;
esac

# Walk up from each file to the nearest Cargo.toml. There is no root workspace.
crates=$(for f in "$@"; do
  d=$(dirname "$f")
  while [[ "$d" != "." && ! -f "$d/Cargo.toml" ]]; do d=$(dirname "$d"); done
  if [[ -f "$d/Cargo.toml" ]]; then echo "$d"; fi
done | sort -u)

for c in $crates; do
  # Crates that only build for a non-host target (eBPF programs) are linted by
  # their own build, so only formatting runs here.
  if [[ "$cmd" != fmt ]] && grep -q '^host-build = false' "$c/Cargo.toml"; then
    echo "==> $c: skip cargo $cmd (host-build = false)"
    continue
  fi
  echo "==> $c: cargo $cmd"
  # cd, not --manifest-path: rustup resolves rust-toolchain.toml from cwd
  case "$cmd" in
    fmt) (cd "$c" && cargo fmt -- --check) ;;
    clippy) (cd "$c" && cargo clippy -- -D warnings) ;;
    test) (cd "$c" && cargo test) ;;
  esac
done
