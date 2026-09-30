#!/usr/bin/env bash
# Decide whether to release the Harbor arm64 images.
# Usage: ci-detect-harbor-release.sh <harbor-dir>
#   Writes release and version to GITHUB_OUTPUT. Releases when any chart image
#   is missing the version tag on GHCR, or on a forced dispatch.
# Env: GH_TOKEN, GITHUB_ACTOR, GITHUB_SHA, GITHUB_OUTPUT, EVENT_NAME,
#   PUSH_BEFORE (push), FORCE (workflow_dispatch).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HARBOR_DIR="${1:?Usage: ci-detect-harbor-release.sh <harbor-dir>}"
VERSION_FILE="${HARBOR_DIR}/VERSION"
FORCE="${FORCE:-false}"

skip() {
  echo "release=false" >> "${GITHUB_OUTPUT}"
  echo "version=${1:-}" >> "${GITHUB_OUTPUT}"
  exit 0
}

# A push releases only when it touched VERSION. Editing the builder alone runs
# the workflow and no-ops, like a Dockerfile edit without a label bump.
if [[ "${EVENT_NAME}" == "push" ]]; then
  CHANGED=$("${SCRIPT_DIR}/ci-changed-files.sh")
  echo "Changed files:"
  echo "${CHANGED}"
  if ! grep -qxF "${VERSION_FILE}" <<< "${CHANGED}"; then
    echo "${VERSION_FILE} unchanged, skip"
    skip
  fi
fi

version=$(tr -d '[:space:]' < "${VERSION_FILE}")
if [[ ! "${version}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error::invalid Harbor version '${version}' in ${VERSION_FILE}"
  exit 1
fi

# Assigned first so a failed build or GHCR lookup aborts instead of reading as
# "nothing to check" or "missing".
images=$(cargo run --quiet --locked --manifest-path "${HARBOR_DIR}/Cargo.toml" -- images)
[[ -n "${images}" ]] || { echo "::error::harbor-arm64 listed no images"; exit 1; }

missing=0
while read -r image; do
  status=$("${SCRIPT_DIR}/ci-ghcr-tag-status.sh" "younsl/harbor/${image}" "${version}")
  if [[ "${status}" == "exists" ]]; then
    echo "✅ harbor/${image}:${version} exists on GHCR"
  else
    echo "🆕 harbor/${image}:${version} not found on GHCR"
    missing=$((missing + 1))
  fi
done <<< "${images}"

if [[ "${missing}" -gt 0 ]]; then
  echo "${missing} image(s) missing, will release ${version}"
elif [[ "${FORCE}" == "true" ]]; then
  echo "♻️ all images exist, forced rebuild, the tags will be overwritten"
else
  echo "All images exist, skip"
  skip "${version}"
fi

echo "release=true" >> "${GITHUB_OUTPUT}"
echo "version=${version}" >> "${GITHUB_OUTPUT}"
