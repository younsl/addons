#!/usr/bin/env bash
# Pick the Dockerfile-built container images to release and emit the job matrices.
# Usage: ci-detect-container-releases.sh   Writes has_releases, build_matrix,
#   manifest_matrix to GITHUB_OUTPUT.
# Env: GH_TOKEN, GITHUB_ACTOR, GITHUB_SHA, GITHUB_OUTPUT, EVENT_NAME,
#   PUSH_BEFORE (push), SELECTED and FORCE (workflow_dispatch).
# Images are versioned by directory: box/addons/<project>/<directory>/Dockerfile.
# SELECTED is the <directory> to release.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECTS=(jenkins-inbound-agent)
SELECTED="${SELECTED:-}"
FORCE="${FORCE:-false}"

CHANGED=$("${SCRIPT_DIR}/ci-changed-files.sh")
if [[ "${EVENT_NAME}" == "push" ]]; then
  echo "Changed files:"
  echo "${CHANGED}"
fi

RELEASES="[]"

for project in "${PROJECTS[@]}"; do
  for dockerfile in box/addons/"${project}"/*/Dockerfile; do
    [[ -f "${dockerfile}" ]] || continue
    base_dir=$(dirname "${dockerfile}")
    directory=$(basename "${base_dir}")

    if [[ -n "${SELECTED}" && "${SELECTED}" != "${directory}" ]]; then
      continue
    fi

    # A push releases only what it touched; the GHCR check cannot tell an
    # unchanged project from one whose lookup failed.
    if [[ "${EVENT_NAME}" == "push" ]] && ! grep -qxF "${dockerfile}" <<< "${CHANGED}"; then
      continue
    fi

    version=$(sed -nE 's/.*org\.opencontainers\.image\.version="([^"]+)".*/\1/p' "${dockerfile}" | head -1)
    if [[ -z "${version}" ]]; then
      echo "::error::org.opencontainers.image.version not found in ${dockerfile}"
      exit 1
    fi

    status=$("${SCRIPT_DIR}/ci-ghcr-tag-status.sh" "younsl/${project}" "${version}")
    if [[ "${status}" == "exists" ]]; then
      if [[ "${FORCE}" != "true" ]]; then
        echo "✅ ${project}:${version} already exists on GHCR, skip"
        continue
      fi
      echo "♻️ ${project}:${version} exists on GHCR, forced rebuild, the tag will be overwritten"
    else
      echo "🆕 ${project}:${version} not found on GHCR, will release"
    fi

    RELEASES=$(jq -c \
      --arg project "${project}" \
      --arg directory "${directory}" \
      --arg base_dir "${base_dir}" \
      --arg version "${version}" \
      '. + [{project: $project, directory: $directory, base_dir: $base_dir, image: ("younsl/" + $project), version: $version}]' \
      <<< "${RELEASES}")
  done
done

if [[ -n "${SELECTED}" && "$(jq 'length' <<< "${RELEASES}")" -eq 0 && "${FORCE}" == "true" ]]; then
  echo "::error::no Dockerfile matched directory ${SELECTED}"
  exit 1
fi

BUILD_JSON=$(jq -c '[ .[] as $p
  | {"amd64": "ubuntu-26.04", "arm64": "ubuntu-26.04-arm"}
  | to_entries[]
  | $p + {arch: .key, runner: .value} ]' <<< "${RELEASES}")

HAS_RELEASES="false"
[[ "$(jq 'length' <<< "${RELEASES}")" -gt 0 ]] && HAS_RELEASES="true"

{
  echo "has_releases=${HAS_RELEASES}"
  echo "build_matrix={\"include\":${BUILD_JSON}}"
  echo "manifest_matrix={\"include\":${RELEASES}}"
} >> "${GITHUB_OUTPUT}"

echo "build_matrix: ${BUILD_JSON}"
echo "manifest_matrix: ${RELEASES}"
