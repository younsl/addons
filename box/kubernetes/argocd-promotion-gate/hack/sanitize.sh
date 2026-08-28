#!/usr/bin/env bash
# Sanitize public identity references before mirroring to a private repo.
#
# The old values are read out of Cargo.toml, Chart.yaml, and values.yaml at runtime,
# so this script holds no identity string of its own and is safe to mirror.
# Generic registry fixtures (example/, stefanprodan/) are intentionally kept.
#
# Usage:
#   ./hack/sanitize.sh              # dry-run: show what would change
#   ./hack/sanitize.sh --apply      # rewrite files in place
#
# Override replacement targets via environment variables:
#   NEW_MODULE=git.example.com/platform/argocd-promotion-gate \
#   NEW_REGISTRY=registry.example.com/platform \
#   NEW_SOURCE_URL=https://git.example.com/platform/argocd-promotion-gate \
#   NEW_DOMAIN=promotion-gate.example.com \
#   ./hack/sanitize.sh --apply
set -euo pipefail

cd "$(dirname "$0")/.."

BINARY="argocd-promotion-gate"
CHART="charts/${BINARY}/Chart.yaml"
VALUES="charts/${BINARY}/values.yaml"

APPLY=false
[[ "${1:-}" == "--apply" ]] && APPLY=true

# --- old values, derived from the repo itself -------------------------------

yaml_value() { # file, key regex -> first scalar value, quotes and list dash stripped
  sed -nE "s|^[[:space:]]*(- )?$2:[[:space:]]*[\"']?([^\"']+)[\"']?[[:space:]]*$|\2|p" "$1" | head -1
}

# Cargo.toml's repository URL stands in for the Go module path: the host and
# namespace it carries are the identity strings that leak into docs and chart.
OLD_REPO_URL=$(sed -nE 's|^repository[[:space:]]*=[[:space:]]*"([^"]+)".*|\1|p' Cargo.toml | head -1)
OLD_MODULE="${OLD_REPO_URL#*://}/${BINARY}"
OLD_MODULE_PREFIX="${OLD_MODULE%/*}"
OLD_NAMESPACE=$(cut -d/ -f2 <<<"$OLD_MODULE")

OLD_REGISTRY_HOST=$(yaml_value "$VALUES" registry)
OLD_REPOSITORY=$(yaml_value "$VALUES" repository)
OLD_REGISTRY="${OLD_REGISTRY_HOST}/${OLD_REPOSITORY%%/*}"

OLD_HOME=$(yaml_value "$CHART" home)
OLD_SOURCE=$(sed -nE 's|^[[:space:]]*- (https?://.*)$|\1|p' "$CHART" | head -1)
OLD_MAINTAINER=$(yaml_value "$CHART" name | tail -1)
OLD_MAINTAINER_EMAIL=$(yaml_value "$CHART" email)
OLD_MAINTAINER_URL=$(yaml_value "$CHART" url)
OLD_DOMAIN=$(sed -nE "s|.*[[:space:]]([A-Za-z0-9.-]*${OLD_NAMESPACE}\.[A-Za-z0-9.-]+)/.*|\1|p" "$VALUES" | head -1)
OLD_DOMAIN="${OLD_DOMAIN#*.}"

for v in OLD_MODULE OLD_NAMESPACE OLD_REGISTRY_HOST OLD_REPOSITORY OLD_HOME \
         OLD_SOURCE OLD_MAINTAINER OLD_MAINTAINER_EMAIL OLD_MAINTAINER_URL OLD_DOMAIN; do
  [[ -n "${!v}" ]] || { echo "FAIL: could not derive $v from the repo" >&2; exit 1; }
done

# --- new values -------------------------------------------------------------

NEW_MODULE="${NEW_MODULE:-git.example.com/platform/${BINARY}}"
NEW_REGISTRY="${NEW_REGISTRY:-registry.example.com/platform}"
NEW_SOURCE_URL="${NEW_SOURCE_URL:-https://git.example.com/platform/${BINARY}}"
NEW_DOMAIN="${NEW_DOMAIN:-example.com}"
NEW_MAINTAINER="${NEW_MAINTAINER:-platform}"
NEW_MAINTAINER_EMAIL="${NEW_MAINTAINER_EMAIL:-platform@example.com}"
NEW_MAINTAINER_URL="${NEW_MAINTAINER_URL:-https://git.example.com/platform}"

NEW_MODULE_PREFIX="${NEW_MODULE%/*}"
REGISTRY_HOST="${NEW_REGISTRY%%/*}"
REGISTRY_NS="${NEW_REGISTRY#*/}"

# --- rules ------------------------------------------------------------------

# pattern|replacement (longest / most specific first)
RULES=(
  "${OLD_SOURCE}|${NEW_SOURCE_URL}"
  "${OLD_MODULE_PREFIX}|${NEW_MODULE_PREFIX}"
  "${OLD_HOME}|${NEW_SOURCE_URL}"
  "${OLD_MAINTAINER_URL}|${NEW_MAINTAINER_URL}"
  "${OLD_REGISTRY}|${NEW_REGISTRY}"
  "${OLD_REPOSITORY}|${REGISTRY_NS}/${BINARY}"
  "${OLD_DOMAIN}|${NEW_DOMAIN}"
  "${OLD_MAINTAINER_EMAIL}|${NEW_MAINTAINER_EMAIL}"
  "registry: ${OLD_REGISTRY_HOST}|registry: ${REGISTRY_HOST}"
  "\"${OLD_REGISTRY_HOST}\"|\"${REGISTRY_HOST}\""
  # catch-all: the bare namespace left in maintainer fields
  "${OLD_NAMESPACE}|${NEW_MAINTAINER}"
)

# Lines to delete outright (badges pointing at the public repo / registry)
DELETE_PATTERNS=(
  "img.shields.io"
)

escape_re() { sed 's/[].[^$*\\/]/\\&/g' <<<"$1"; }

# Anything matching this is an identity leak, unless it also matches ALLOW_PATTERN.
CHECK_PATTERN="$(escape_re "$OLD_NAMESPACE")|$(escape_re "$OLD_REGISTRY_HOST")|$(escape_re "$OLD_MAINTAINER_EMAIL")"
ALLOW_PATTERN="$(escape_re "$OLD_REGISTRY_HOST")/(example|stefanprodan)"

matching_lines() {
  grep -rInE \
    --exclude-dir=.git --exclude-dir=bin --exclude-dir=node_modules \
    --exclude=Cargo.lock --exclude-dir=target \
    "$CHECK_PATTERN" . 2>/dev/null | grep -vE "$ALLOW_PATTERN" || true
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
