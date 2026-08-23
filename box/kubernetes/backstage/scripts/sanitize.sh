#!/usr/bin/env bash
# Sanitize public identity references before mirroring to a private repo.
#
# The old values are read out of values.yaml and the Dockerfile OCI labels at
# runtime, so this script holds no identity string of its own and is safe to
# mirror. Upstream OSS links and generic registry fixtures (ghcr.io/org/...)
# are intentionally kept.
#
# Usage:
#   ./scripts/sanitize.sh              # dry-run: show what would change
#   ./scripts/sanitize.sh --apply      # rewrite files in place
#
# Override replacement targets via environment variables:
#   NEW_REGISTRY=harbor.example.com/backstage \
#   NEW_SOURCE_URL=https://git.example.com/platform/backstage \
#   ./scripts/sanitize.sh --apply
set -euo pipefail

cd "$(dirname "$0")/.."

VALUES="values.yaml"

APPLY=false
[[ "${1:-}" == "--apply" ]] && APPLY=true

# --- old values, derived from the repo itself -------------------------------

yaml_value() { # file, key -> first scalar value, quotes stripped
  sed -nE "s|^[[:space:]]*$2:[[:space:]]*[\"']?([^\"'#]+[^\"' #])[\"']?[[:space:]]*$|\1|p" "$1" | head -1
}

OLD_HOST=$(yaml_value "$VALUES" registry)
OLD_REPOSITORY=$(yaml_value "$VALUES" repository)
OLD_NAMESPACE="${OLD_REPOSITORY%%/*}"
OLD_SOURCE_URL=$(sed -nE 's|.*org\.opencontainers\.image\.source="([^"]+)".*|\1|p' Dockerfile | head -1)

for v in OLD_HOST OLD_REPOSITORY OLD_NAMESPACE OLD_SOURCE_URL; do
  [[ -n "${!v}" ]] || { echo "FAIL: could not derive $v from the repo" >&2; exit 1; }
done
[[ "$OLD_REPOSITORY" == */* ]] || { echo "FAIL: $VALUES repository has no namespace" >&2; exit 1; }

# --- new values -------------------------------------------------------------

NEW_REGISTRY="${NEW_REGISTRY:-registry.example.com/backstage}"
NEW_SOURCE_URL="${NEW_SOURCE_URL:-https://git.example.com/platform/backstage}"

REGISTRY_HOST="${NEW_REGISTRY%%/*}"
REGISTRY_PATH="${NEW_REGISTRY#*/}"

# --- rules ------------------------------------------------------------------

# pattern|replacement (longest / most specific first)
RULES=(
  "${OLD_HOST}/${OLD_REPOSITORY}|${NEW_REGISTRY}"
  "${OLD_HOST}/${OLD_NAMESPACE}|${REGISTRY_HOST}"
  "${OLD_SOURCE_URL}|${NEW_SOURCE_URL}"
  "${OLD_REPOSITORY}|${REGISTRY_PATH}"
  "registry: ${OLD_HOST}|registry: ${REGISTRY_HOST}"
)

# Lines to delete outright (badges pointing at the public registry)
DELETE_PATTERNS=(
  "img.shields.io"
)

escape_re() { sed 's/[].[^$*\\/]/\\&/g' <<<"$1"; }

# An identity leak is the namespace itself, or the public registry named as the
# image source. A bare host in a test fixture (ghcr.io/org/...) is not a leak.
CHECK_PATTERN="$(escape_re "$OLD_NAMESPACE")|registry: $(escape_re "$OLD_HOST")"

matching_lines() {
  grep -rInE \
    --exclude-dir=node_modules --exclude-dir=node_modules.bak \
    --exclude-dir=.yarn --exclude-dir=dist --exclude-dir=dist-types \
    --exclude-dir=.git --exclude=yarn.lock \
    "$CHECK_PATTERN" . 2>/dev/null || true
}

list_files() {
  matching_lines | cut -d: -f1 | sort -u
}

FILES=$(list_files)
if [[ -z "$FILES" ]]; then
  echo "clean: no identity references found"
  exit 0
fi

FILE_COUNT=$(echo "$FILES" | wc -l | tr -d ' ')
echo "== files containing identity references (${FILE_COUNT} files) =="
echo "$FILES"
echo

if ! $APPLY; then
  echo "== dry-run: matching lines (use --apply to rewrite) =="
  matching_lines
  exit 0
fi

for f in $FILES; do
  for pat in "${DELETE_PATTERNS[@]}"; do
    sed -i '' "\|${pat}|d" "$f"
  done
  for rule in "${RULES[@]}"; do
    sed -i '' "s|${rule%%|*}|${rule#*|}|g" "$f"
  done
done

echo "== verification =="
LEFTOVER=$(list_files)
if [[ -n "$LEFTOVER" ]]; then
  echo "FAIL: identity references remain:"
  matching_lines
  exit 1
fi
echo "OK: no identity references remain"
