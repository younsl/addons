#!/usr/bin/env bash
#
# Run a cargo check in every crate that owns one of the given files.
# Called by pre-commit with the changed files as arguments.
#
#   ./scripts/cargo-changed.sh fmt    box/tools/ij/src/main.rs
#   ./scripts/cargo-changed.sh clippy box/tools/ij/src/main.rs
#   ./scripts/cargo-changed.sh test   box/tools/ij/src/main.rs

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
  echo "==> $c: cargo $cmd"
  # cd, not --manifest-path: rustup resolves rust-toolchain.toml from cwd
  case "$cmd" in
    fmt) (cd "$c" && cargo fmt -- --check) ;;
    clippy) (cd "$c" && cargo clippy -- -D warnings) ;;
    test) (cd "$c" && cargo test) ;;
  esac
done
