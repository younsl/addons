#!/usr/bin/env bash
# Sanitize public identity references before mirroring to a private repo.
#
# Every old value is derived from the repo at runtime (Cargo.toml, Chart.yaml,
# values.yaml, Makefile), so this script carries no identity string of its own
# and is safe to commit and to mirror. Upstream OSS links (argoproj, helm-docs,
# go-containerregistry) are intentionally kept.
#
# Usage:
#   ./scripts/sanitize.sh                  # dry-run: report what would change
#   ./scripts/sanitize.sh --apply          # rewrite this working tree in place
#   ./scripts/sanitize.sh --mirror <dir>   # export tracked files, sanitize, sync to <dir>
#
# --mirror never touches the source tree: it stages `git ls-files` output in a
# temp dir, sanitizes there, then rsyncs into <dir>.
#
# Override replacement targets via environment variables:
#   NEW_REGISTRY=registry.example.com \
#   NEW_REPOSITORY=argocd-canary-gate \
#   NEW_CHART_REGISTRY=registry.example.com/charts \
#   NEW_DOMAIN=example.com \  # empty keeps the public annotation domain
#   NEW_MAINTAINER="Platform Team" \
#   NEW_MAINTAINER_EMAIL=platform@example.com \
#   ./scripts/sanitize.sh --mirror ../mirror/argocd-canary-gate
set -euo pipefail

SRC_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

BINARY="argocd-canary-gate"
CHART_REL="charts/${BINARY}"

MODE=report
MIRROR_DEST=""
case "${1:-}" in
  --apply)  MODE=apply ;;
  --mirror) MODE=mirror; MIRROR_DEST="${2:?--mirror needs a target directory}" ;;
  "")       ;;
  *)        echo "unknown argument: $1" >&2; exit 2 ;;
esac

# --- new values -------------------------------------------------------------

NEW_REGISTRY="${NEW_REGISTRY:-registry.example.com}"
NEW_REPOSITORY="${NEW_REPOSITORY:-${BINARY}}"
NEW_CHART_REGISTRY="${NEW_CHART_REGISTRY:-${NEW_REGISTRY}/charts}"
# Empty value keeps the public domain: an annotation key already in use on
# live Applications cannot be renamed without a migration.
NEW_DOMAIN="${NEW_DOMAIN-example.com}"
NEW_MAINTAINER="${NEW_MAINTAINER:-platform}"
NEW_MAINTAINER_EMAIL="${NEW_MAINTAINER_EMAIL:-platform@example.com}"

# --- old values, derived from the repo itself -------------------------------

derive() { # run inside a repo copy; exports OLD_*
  local chart="${CHART_REL}/Chart.yaml" values="${CHART_REL}/values.yaml"

  yaml_value() { # file, key -> first scalar value, quotes and list dash stripped
    sed -nE "s|^[[:space:]]*(- )?$2:[[:space:]]*[\"']?([^\"'#]+[^\"' #])[\"']?[[:space:]]*\$|\2|p" "$1" | head -1
  }

  # Cargo.toml carries the public repository URL and the author identity.
  OLD_REPO_URL=$(sed -nE 's|^repository[[:space:]]*=[[:space:]]*"([^"]+)".*|\1|p' Cargo.toml | head -1)
  OLD_AUTHOR=$(sed -nE 's|^authors[[:space:]]*=[[:space:]]*\["([^"]+)".*|\1|p' Cargo.toml | head -1)
  OLD_SOURCE_PREFIX="${OLD_REPO_URL#*://}"
  OLD_NAMESPACE=$(cut -d/ -f2 <<<"$OLD_SOURCE_PREFIX")

  OLD_HOME=$(yaml_value "$chart" home)
  OLD_SOURCE_URL=$(sed -nE 's|^[[:space:]]*- (https?://.*)$|\1|p' "$chart" | head -1)
  OLD_MAINTAINER=$(sed -nE 's|^[[:space:]]*- name:[[:space:]]*(.+)$|\1|p' "$chart" | head -1)
  OLD_MAINTAINER_EMAIL=$(yaml_value "$chart" email)
  OLD_MAINTAINER_URL=$(yaml_value "$chart" url)

  OLD_REGISTRY=$(yaml_value "$values" registry)
  OLD_REPOSITORY=$(yaml_value "$values" repository)
  OLD_CHART_REGISTRY="${OLD_REGISTRY}/${OLD_NAMESPACE}/charts"
  # Everything left of the image name: what the Makefile's REGISTRY holds.
  if [[ "$OLD_REPOSITORY" == */* ]]; then
    OLD_IMAGE_PREFIX="${OLD_REGISTRY}/${OLD_REPOSITORY%/*}"
  else
    OLD_IMAGE_PREFIX="${OLD_REGISTRY}"
  fi

  # The gate's own annotation key carries the public domain:
  # <component>.<domain>/<key>. Drop the key and the leading component label.
  OLD_DOMAIN=$(yaml_value "$values" annotation)
  OLD_DOMAIN="${OLD_DOMAIN%%/*}"
  OLD_DOMAIN="${OLD_DOMAIN#*.}"

  local v
  for v in OLD_REPO_URL OLD_AUTHOR OLD_NAMESPACE OLD_HOME OLD_SOURCE_URL \
           OLD_MAINTAINER OLD_MAINTAINER_EMAIL OLD_MAINTAINER_URL \
           OLD_REGISTRY OLD_REPOSITORY OLD_DOMAIN; do
    [[ -n "${!v}" ]] || { echo "FAIL: could not derive $v from the repo" >&2; exit 1; }
  done
  [[ "$OLD_DOMAIN" == *"$OLD_NAMESPACE"* ]] \
    || { echo "FAIL: derived domain '$OLD_DOMAIN' carries no identity" >&2; exit 1; }
}

cd "$SRC_ROOT"
derive

# --- rules ------------------------------------------------------------------

if [[ "$NEW_REPOSITORY" == */* ]]; then
  NEW_IMAGE_PREFIX="${NEW_REGISTRY}/${NEW_REPOSITORY%/*}"
else
  NEW_IMAGE_PREFIX="${NEW_REGISTRY}"
fi

TAB=$'\t'

# pattern<TAB>replacement, longest / most specific first. Applied with sed s%..%..%g,
# so neither side may contain '%'; the table below never needs one.
RULES=(
  "${OLD_HOME}/blob/main/LICENSE${TAB}LICENSE"
  "${OLD_CHART_REGISTRY}${TAB}${NEW_CHART_REGISTRY}"
  "${OLD_REGISTRY}/${OLD_REPOSITORY}${TAB}${NEW_REGISTRY}/${NEW_REPOSITORY}"
  "${OLD_IMAGE_PREFIX}${TAB}${NEW_IMAGE_PREFIX}"
  "repository: ${OLD_REPOSITORY}${TAB}repository: ${NEW_REPOSITORY}"
  "\"${OLD_REPOSITORY}\"${TAB}\"${NEW_REPOSITORY}\""
  "registry: ${OLD_REGISTRY}${TAB}registry: ${NEW_REGISTRY}"
  "\"${OLD_REGISTRY}\"${TAB}\"${NEW_REGISTRY}\""
  "${OLD_AUTHOR}${TAB}${NEW_MAINTAINER} <${NEW_MAINTAINER_EMAIL}>"
  "name: ${OLD_MAINTAINER}${TAB}name: ${NEW_MAINTAINER}"
  "| ${OLD_MAINTAINER} |${TAB}| ${NEW_MAINTAINER} |"
  " <${OLD_MAINTAINER_URL}> ${TAB} "
  "${OLD_MAINTAINER_EMAIL}${TAB}${NEW_MAINTAINER_EMAIL}"
)

KEEP_DOMAIN=false
if [[ -z "$NEW_DOMAIN" || "$NEW_DOMAIN" == "$OLD_DOMAIN" ]]; then
  KEEP_DOMAIN=true
else
  RULES+=("${OLD_DOMAIN}${TAB}${NEW_DOMAIN}")
fi

# Whole lines to drop: the public repository pointer, the chart's home/sources
# block, the maintainer profile URL, and badges aimed at the public registry.
DELETE_PATTERNS=(
  "^repository = \"${OLD_REPO_URL}\"$"
  "^home: ${OLD_HOME}$"
  "^sources:$"
  "^  - ${OLD_REPO_URL}"
  "^    url: ${OLD_MAINTAINER_URL}$"
  "img\.shields\.io.*(${OLD_NAMESPACE}|${OLD_REGISTRY})"
)

# helm-docs sections that only make sense on the public repo, with the blank
# line that follows them.
strip_blocks() {
  perl -0777 -i -pe '
    s/\*\*Homepage:\*\* <[^\n]*>\n\n//g;
    s/## Source Code\n\n(\* <[^\n]*>\n)+\n//g;
  ' "$@"
}

escape_re() { sed 's/[].[^$*\\/]/\\&/g' <<<"$1"; }

# Anything matching this is an identity leak. Upstream OSS links carry none of
# these, so they survive untouched.
CHECK_PATTERN="$(escape_re "$OLD_NAMESPACE")|$(escape_re "$OLD_REGISTRY")|$(escape_re "$OLD_MAINTAINER_EMAIL")|$(escape_re "$OLD_DOMAIN")"

raw_matches() {
  grep -rInE \
    --exclude-dir=.git --exclude-dir=target --exclude-dir=node_modules \
    --exclude=Cargo.lock --exclude="$(basename "$0")" \
    "$CHECK_PATTERN" . 2>/dev/null || true
}

# A kept annotation domain is not a leak, but the namespace inside it would
# otherwise match, so strip it before deciding whether the line still leaks.
matching_lines() {
  if ! $KEEP_DOMAIN; then raw_matches; return; fi
  local line
  raw_matches | while IFS= read -r line; do
    grep -qE "$CHECK_PATTERN" <<<"${line//${OLD_DOMAIN}/}" && printf '%s\n' "$line"
  done
  return 0
}

list_files() { matching_lines | cut -d: -f1 | sort -u; }

rewrite_tree() { # run inside the tree to sanitize
  local files f pat rule
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
  strip_blocks README.md "${CHART_REL}/README.md"

  local leftover
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
rsync -a --delete --exclude='.git/' "$STAGE"/ "$MIRROR_DEST"/
echo "mirrored ${COPIED} tracked files to ${MIRROR_DEST}"
