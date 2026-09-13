#!/usr/bin/env bash
# Decide which Dockerfile-built containers to release.
# Usage: ci-detect-container-releases.sh <name:dockerfile> [...]
#   Writes <name>_release and <name>_version (dashes become underscores) to
#   GITHUB_OUTPUT.
# Env: GH_TOKEN, GITHUB_ACTOR, GITHUB_SHA, GITHUB_OUTPUT, EVENT_NAME,
#   PUSH_BEFORE (push), SELECTED and FORCE (workflow_dispatch).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELECTED="${SELECTED:-}"
FORCE="${FORCE:-false}"

CHANGED=$("${SCRIPT_DIR}/ci-changed-files.sh")
if [[ "${EVENT_NAME}" == "push" ]]; then
  echo "Changed files:"
  echo "${CHANGED}"
fi

skip() {
  echo "$1_release=false" >> "${GITHUB_OUTPUT}"
  echo "$1_version=" >> "${GITHUB_OUTPUT}"
}

for entry in "$@"; do
  IFS=':' read -r container dockerfile <<< "${entry}"
  var="${container//-/_}"

  if [[ -n "${SELECTED}" && "${SELECTED}" != "${container}" ]]; then
    skip "${var}"
    continue
  fi

  # A push releases only what it touched; the GHCR check cannot tell an
  # unchanged container from one whose lookup failed.
  if [[ "${EVENT_NAME}" == "push" ]] && ! grep -qxF "${dockerfile}" <<< "${CHANGED}"; then
    skip "${var}"
    continue
  fi

  if [[ ! -f "${dockerfile}" ]]; then
    echo "::warning::${dockerfile} not found, skipping"
    skip "${var}"
    continue
  fi

  version=$(sed -nE 's/.*org\.opencontainers\.image\.version="([^"]+)".*/\1/p' "${dockerfile}" | head -1)
  if [[ -z "${version}" ]]; then
    echo "::error::org.opencontainers.image.version not found in ${dockerfile}"
    skip "${var}"
    continue
  fi

  if [[ "$("${SCRIPT_DIR}/ci-ghcr-tag-status.sh" "younsl/${container}" "${version}")" == "exists" ]]; then
    if [[ "${FORCE}" == "true" ]]; then
      echo "♻️ ${container}:${version} exists on GHCR, forced rebuild, the tag will be overwritten"
      echo "${var}_release=true" >> "${GITHUB_OUTPUT}"
    else
      echo "✅ ${container}:${version} already exists on GHCR, skip"
      echo "${var}_release=false" >> "${GITHUB_OUTPUT}"
    fi
  else
    echo "🆕 ${container}:${version} not found on GHCR, will release"
    echo "${var}_release=true" >> "${GITHUB_OUTPUT}"
  fi
  echo "${var}_version=${version}" >> "${GITHUB_OUTPUT}"
done
