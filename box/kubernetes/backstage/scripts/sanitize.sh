#!/usr/bin/env bash
# Sanitize public identity references before mirroring to a private repo.
#
# Every old value is derived from the repo at runtime (values.yaml, Dockerfile
# OCI labels, and the backstage-mcp Cargo.toml / Chart.yaml), so this script
# holds no identity string of its own and is safe to commit. Upstream OSS links
# and generic registry fixtures (ghcr.io/org/...) are intentionally kept.
#
# Usage:
#   ./scripts/sanitize.sh                  # dry-run: show what would change
#   ./scripts/sanitize.sh --apply          # rewrite this working tree in place
#   ./scripts/sanitize.sh --mirror <dir>   # export tracked files, sanitize, sync to <dir>
#
# --mirror never touches the source tree: it stages `git ls-files` output in a
# temp dir, sanitizes there, then rsyncs into <dir>. backstage-mcp/ ships inside
# the same component, so it is sanitized against the same replacement targets.
#
# Override replacement targets via environment variables:
#   NEW_REGISTRY=harbor.example.com/backstage \
#   NEW_SOURCE_URL=https://git.example.com/platform/backstage \
#   NEW_MAINTAINER="Platform Team" \
#   NEW_MAINTAINER_EMAIL=platform@example.com \
#   ./scripts/sanitize.sh --mirror ../mirror/backstage
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$SRC_ROOT"

VALUES="values.yaml"
MCP_DIR="backstage-mcp"
MCP_CHART="${MCP_DIR}/charts/backstage-mcp"

MODE=report
MIRROR_DEST=""
case "${1:-}" in
  --apply)  MODE=apply ;;
  --mirror) MODE=mirror; MIRROR_DEST="${2:?--mirror needs a target directory}" ;;
  "")       ;;
  *)        echo "unknown argument: $1" >&2; exit 2 ;;
esac

# The mirror clone is a working checkout too, so `rsync --delete` must leave its
# build artifacts and local-only config alone.
RSYNC_EXCLUDES=(
  '.git/'
  'node_modules/'
  'dist/'
  'dist-types/'
  'target/'
  '.env'
  'app-config.local.yaml'
  '.yarn/install-state.gz'
  'packages/app/src/buildInfo.ts'
)

# --- old values, derived from the repo itself -------------------------------

yaml_value() { # file, key -> first scalar value, quotes and list dash stripped
  sed -nE "s|^[[:space:]]*(- )?$2:[[:space:]]*[\"']?([^\"'#]+[^\"' #])[\"']?[[:space:]]*$|\2|p" "$1" | head -1
}

OLD_HOST=$(yaml_value "$VALUES" registry)
OLD_REPOSITORY=$(yaml_value "$VALUES" repository)
OLD_NAMESPACE="${OLD_REPOSITORY%%/*}"
OLD_SOURCE_URL=$(sed -nE 's|.*org\.opencontainers\.image\.source="([^"]+)".*|\1|p' Dockerfile | head -1)

# backstage-mcp carries a Rust package and a chart of its own.
OLD_MCP_REPO_URL=$(sed -nE 's|^repository[[:space:]]*=[[:space:]]*"([^"]+)".*|\1|p' "${MCP_DIR}/Cargo.toml" | head -1)
OLD_MCP_HOMEPAGE=$(sed -nE 's|^homepage[[:space:]]*=[[:space:]]*"([^"]+)".*|\1|p' "${MCP_DIR}/Cargo.toml" | head -1)
OLD_AUTHOR=$(sed -nE 's|^authors[[:space:]]*=[[:space:]]*\["([^"]+)".*|\1|p' "${MCP_DIR}/Cargo.toml" | head -1)
OLD_HOME=$(yaml_value "${MCP_CHART}/Chart.yaml" home)
OLD_CHART_SOURCE=$(sed -nE 's|^[[:space:]]*- (https?://.*)$|\1|p' "${MCP_CHART}/Chart.yaml" | head -1)
OLD_MAINTAINER=$(sed -nE 's|^[[:space:]]*- name:[[:space:]]*(.+)$|\1|p' "${MCP_CHART}/Chart.yaml" | head -1)
OLD_MAINTAINER_EMAIL=$(yaml_value "${MCP_CHART}/Chart.yaml" email)
OLD_MAINTAINER_URL=$(yaml_value "${MCP_CHART}/Chart.yaml" url)
OLD_CHART_REGISTRY="${OLD_HOST}/${OLD_NAMESPACE}/charts"

for v in OLD_HOST OLD_REPOSITORY OLD_NAMESPACE OLD_SOURCE_URL \
         OLD_MCP_REPO_URL OLD_MCP_HOMEPAGE OLD_AUTHOR \
         OLD_HOME OLD_CHART_SOURCE OLD_MAINTAINER OLD_MAINTAINER_EMAIL OLD_MAINTAINER_URL; do
  [[ -n "${!v}" ]] || { echo "FAIL: could not derive $v from the repo" >&2; exit 1; }
done
[[ "$OLD_REPOSITORY" == */* ]] || { echo "FAIL: $VALUES repository has no namespace" >&2; exit 1; }

# --- new values -------------------------------------------------------------

NEW_REGISTRY="${NEW_REGISTRY:-registry.example.com/backstage}"
NEW_SOURCE_URL="${NEW_SOURCE_URL:-https://git.example.com/platform/backstage}"
NEW_MAINTAINER="${NEW_MAINTAINER:-platform}"
NEW_MAINTAINER_EMAIL="${NEW_MAINTAINER_EMAIL:-platform@example.com}"

REGISTRY_HOST="${NEW_REGISTRY%%/*}"
REGISTRY_PATH="${NEW_REGISTRY#*/}"
NEW_CHART_REGISTRY="${NEW_CHART_REGISTRY:-${REGISTRY_HOST}/charts}"

# --- rules ------------------------------------------------------------------

TAB=$'\t'

# pattern<TAB>replacement, longest / most specific first. Applied with sed
# s%..%..%g, so neither side may contain '%'.
RULES=(
  "${OLD_HOME}/blob/main/LICENSE${TAB}LICENSE"
  "${OLD_CHART_REGISTRY}${TAB}${NEW_CHART_REGISTRY}"
  "${OLD_HOST}/${OLD_REPOSITORY}${TAB}${NEW_REGISTRY}"
  "${OLD_HOST}/${OLD_NAMESPACE}${TAB}${REGISTRY_HOST}"
  "${OLD_SOURCE_URL}${TAB}${NEW_SOURCE_URL}"
  "${OLD_REPOSITORY}${TAB}${REGISTRY_PATH}"
  "registry: ${OLD_HOST}${TAB}registry: ${REGISTRY_HOST}"
  "${OLD_AUTHOR}${TAB}${NEW_MAINTAINER} <${NEW_MAINTAINER_EMAIL}>"
  "name: ${OLD_MAINTAINER}${TAB}name: ${NEW_MAINTAINER}"
  "| ${OLD_MAINTAINER} |${TAB}| ${NEW_MAINTAINER} |"
  " <${OLD_MAINTAINER_URL}> ${TAB} "
  "${OLD_MAINTAINER_EMAIL}${TAB}${NEW_MAINTAINER_EMAIL}"
)

# Whole lines to drop: the public repository pointers, the chart's home/sources
# block, the maintainer profile URL, and badges aimed at the public identity.
DELETE_PATTERNS=(
  "^repository = \"${OLD_MCP_REPO_URL}\"$"
  "^homepage = \"${OLD_MCP_HOMEPAGE}\"$"
  "^home: ${OLD_HOME}$"
  "^sources:$"
  "^  - ${OLD_CHART_SOURCE}"
  "^    url: ${OLD_MAINTAINER_URL}$"
  "img\.shields\.io.*(${OLD_NAMESPACE}|${OLD_HOST})"
)

# helm-docs sections that only make sense on the public repo, with the blank
# line that follows them.
strip_blocks() {
  local f
  for f in "$@"; do
    [[ -f "$f" ]] || continue
    perl -0777 -i -pe '
      s/\*\*Homepage:\*\* <[^\n]*>\n\n//g;
      s/## Source Code\n\n(\* <[^\n]*>\n)+\n//g;
    ' "$f"
  done
}

escape_re() { sed 's/[].[^$*\\/]/\\&/g' <<<"$1"; }

# An identity leak is the namespace itself, the public registry named as the
# image source, or the maintainer address. A bare host in a test fixture
# (ghcr.io/org/...) is not a leak.
CHECK_PATTERN="$(escape_re "$OLD_NAMESPACE")|registry: $(escape_re "$OLD_HOST")|$(escape_re "$OLD_MAINTAINER_EMAIL")"

matching_lines() {
  grep -rInE \
    --exclude-dir=node_modules --exclude-dir=node_modules.bak \
    --exclude-dir=.yarn --exclude-dir=dist --exclude-dir=dist-types \
    --exclude-dir=target --exclude-dir=.git \
    --exclude=yarn.lock --exclude=Cargo.lock \
    "$CHECK_PATTERN" . 2>/dev/null || true
}

list_files() {
  matching_lines | cut -d: -f1 | sort -u
}

rewrite_tree() { # run inside the tree to sanitize
  local files f pat rule leftover
  files=$(list_files)
  [[ -n "$files" ]] || { echo "clean: no identity references found"; return 0; }

  for f in $files; do
    for pat in "${DELETE_PATTERNS[@]}"; do
      sed -i '' -E "\%${pat}%d" "$f"
    done
    for rule in "${RULES[@]}"; do
      sed -i '' "s%${rule%%${TAB}*}%${rule#*${TAB}}%g" "$f"
    done
  done
  strip_blocks "${MCP_DIR}/README.md" "${MCP_CHART}/README.md"

  leftover=$(list_files)
  if [[ -n "$leftover" ]]; then
    echo "FAIL: identity references remain:" >&2
    matching_lines >&2
    return 1
  fi
  echo "OK: no identity references remain"
}

# --- run --------------------------------------------------------------------

if [[ "$MODE" == report ]]; then
  FILES=$(list_files)
  [[ -n "$FILES" ]] || { echo "clean: no identity references found"; exit 0; }
  echo "== files containing identity references ($(wc -l <<<"$FILES" | tr -d ' ') files) =="
  echo "$FILES"
  echo
  echo "== replacements =="
  printf '%s\n' "${RULES[@]}" | sed "s|${TAB}| -> |"
  echo
  echo "== dry-run: matching lines (use --apply or --mirror to rewrite) =="
  matching_lines
  exit 0
fi

if [[ "$MODE" == apply ]]; then
  rewrite_tree
  exit $?
fi

# --mirror: stage the tracked files, sanitize the staging copy, sync it out.
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

SELF="scripts/$(basename "$0")"
COPIED=0
while IFS= read -r -d '' f; do
  [[ "$f" == "$SELF" ]] && continue
  mkdir -p "$STAGE/$(dirname "$f")"
  cp -p "$SRC_ROOT/$f" "$STAGE/$f"
  COPIED=$((COPIED + 1))
done < <(git -C "$SRC_ROOT" ls-files -z .)

cd "$STAGE"
rewrite_tree

mkdir -p "$MIRROR_DEST"
rsync -a --delete "${RSYNC_EXCLUDES[@]/#/--exclude=}" "$STAGE"/ "$MIRROR_DEST"/
echo "mirrored ${COPIED} tracked files to ${MIRROR_DEST}"
