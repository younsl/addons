#!/usr/bin/env bash
#
# Reclaim disk space by removing Go and Rust build artifacts.
#
# Dry run by default. Pass --yes to actually delete.
#
#   ./scripts/clean-build-artifacts.sh              # report only
#   ./scripts/clean-build-artifacts.sh --yes        # delete rust targets + go caches
#   ./scripts/clean-build-artifacts.sh --yes --node # also delete node_modules and dist
#   ./scripts/clean-build-artifacts.sh --yes --local-only  # skip global go/cargo caches

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

APPLY=0
WITH_NODE=0
LOCAL_ONLY=0

for arg in "$@"; do
  case "$arg" in
    --yes|-y)     APPLY=1 ;;
    --node)       WITH_NODE=1 ;;
    --local-only) LOCAL_ONLY=1 ;;
    -h|--help)    sed -n '2,12p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)            echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

TOTAL_KB=0

human() {
  local kb=$1
  if command -v numfmt >/dev/null 2>&1; then
    numfmt --to=iec --from-unit=1024 "$kb"
  else
    awk -v k="$kb" 'BEGIN{
      split("K M G T", u, " ")
      i = 1
      while (k >= 1024 && i < 4) { k /= 1024; i++ }
      printf "%.1f%s", k, u[i]
    }'
  fi
}

size_kb() {
  [ -e "$1" ] || { echo 0; return; }
  du -sk "$1" 2>/dev/null | awk '{print $1}'
}

# report <path> <label>; accumulates size and deletes when --yes is set
report() {
  local path=$1 label=$2 kb
  kb=$(size_kb "$path")
  [ "$kb" -eq 0 ] && return
  TOTAL_KB=$((TOTAL_KB + kb))
  printf '  %8s  %s\n' "$(human "$kb")" "$label"
  if [ "$APPLY" -eq 1 ]; then
    rm -rf "$path"
  fi
}

echo "== Rust: cargo target directories =="
# A target/ directory only counts when its parent holds a Cargo.toml.
while IFS= read -r target; do
  [ -f "$(dirname "$target")/Cargo.toml" ] || continue
  report "$target" "${target#"$ROOT"/}"
done < <(find "$ROOT" -type d -name target -not -path '*/node_modules/*' -prune 2>/dev/null | sort)

echo
echo "== Zig cache (cargo-zigbuild cross builds) =="
report "$ROOT/.zig-cache" ".zig-cache"
while IFS= read -r zc; do
  report "$zc" "${zc#"$ROOT"/}"
done < <(find "$ROOT" -type d -name .zig-cache -not -path '*/node_modules/*' -prune 2>/dev/null | sort)

if [ "$WITH_NODE" -eq 1 ]; then
  echo
  echo "== Node: node_modules and dist =="
  echo "  (reinstall with 'yarn install' before the next Backstage build)"
  while IFS= read -r nm; do
    report "$nm" "${nm#"$ROOT"/}"
  done < <(find "$ROOT" -type d \( -name node_modules -o -name dist \) -prune 2>/dev/null | sort)
fi

if [ "$LOCAL_ONLY" -eq 0 ]; then
  echo
  echo "== Global caches (shared with every project on this machine) =="
  for cache in "$HOME/Library/Caches/go-build" "$HOME/.cache/go-build" "$HOME/.cache/zig"; do
    report "$cache" "~${cache#"$HOME"}"
  done

  # The module cache is stored read-only (0444), so rm -rf fails on it.
  # go clean -modcache is the only supported way to remove it.
  modcache="$(go env GOMODCACHE 2>/dev/null || echo "$HOME/go/pkg/mod")"
  if [ -d "$modcache" ]; then
    kb=$(size_kb "$modcache")
    TOTAL_KB=$((TOTAL_KB + kb))
    printf '  %8s  %s (go clean -modcache)\n' "$(human "$kb")" "~${modcache#"$HOME"}"
    if [ "$APPLY" -eq 1 ]; then
      if command -v go >/dev/null 2>&1; then
        go clean -modcache
      else
        chmod -R u+w "$modcache" && rm -rf "$modcache"
      fi
    fi
  fi
  # Cargo re-downloads crate sources on the next build; the git checkouts too.
  for cache in "$HOME/.cargo/registry/cache" "$HOME/.cargo/registry/src" "$HOME/.cargo/git/checkouts"; do
    report "$cache" "~${cache#"$HOME"}"
  done
fi

echo
if [ "$APPLY" -eq 1 ]; then
  echo "Reclaimed: $(human "$TOTAL_KB")"
else
  echo "Reclaimable: $(human "$TOTAL_KB")  (dry run; re-run with --yes to delete)"
fi

echo
df -h /System/Volumes/Data 2>/dev/null || df -h /
