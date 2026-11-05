#!/usr/bin/env bash
# Pick the Rust scratch-container projects to release and emit the job matrices.
# Usage: ci-detect-rust-releases.sh   Writes has_releases, test_matrix,
#   binary_matrix, docker_matrix to GITHUB_OUTPUT.
# Env: GH_TOKEN, GITHUB_ACTOR, GITHUB_SHA, GITHUB_OUTPUT, EVENT_NAME,
#   PUSH_BEFORE (push), SELECTED and FORCE (workflow_dispatch).
# Projects come from ci-rust-projects.json, keyed by Dockerfile path. Every field
# is optional: project defaults to the directory the Dockerfile sits in, binary to
# project, frontend_manager to none (npm|pnpm otherwise), frontend_dir and
# embed_dir (the directory the crate embeds assets from) to "-".
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TABLE="${SCRIPT_DIR}/ci-rust-projects.json"
SELECTED="${SELECTED:-}"
FORCE="${FORCE:-false}"

CHANGED=$("${SCRIPT_DIR}/ci-changed-files.sh")
if [[ "${EVENT_NAME}" == "push" ]]; then
  echo "Changed files:"
  echo "${CHANGED}"
fi

RELEASES="[]"

while read -r entry; do
  project=$(jq -r '.project' <<< "${entry}")
  dockerfile=$(jq -r '.dockerfile' <<< "${entry}")

  if [[ -n "${SELECTED}" && "${SELECTED}" != "${project}" ]]; then
    continue
  fi

  # A push releases only what it touched; the GHCR check cannot tell an
  # unchanged project from one whose lookup failed.
  if [[ "${EVENT_NAME}" == "push" ]] && ! grep -qxF "${dockerfile}" <<< "${CHANGED}"; then
    continue
  fi

  if [[ ! -f "${dockerfile}" ]]; then
    echo "::warning::${dockerfile} not found, skipping"
    continue
  fi

  version=$(sed -nE 's/.*org\.opencontainers\.image\.version="([^"]+)".*/\1/p' "${dockerfile}" | head -1)
  if [[ -z "${version}" ]]; then
    echo "::error::org.opencontainers.image.version not found in ${dockerfile}"
    continue
  fi

  if [[ "$("${SCRIPT_DIR}/ci-ghcr-tag-status.sh" "younsl/${project}" "${version}")" == "exists" ]]; then
    if [[ "${FORCE}" != "true" ]]; then
      echo "✅ ${project}:${version} already exists on GHCR, skip"
      continue
    fi
    echo "♻️ ${project}:${version} exists on GHCR, forced rebuild, the tag will be overwritten"
  else
    echo "🆕 ${project}:${version} not found on GHCR, will release"
  fi

  RELEASES=$(jq -c \
    --argjson entry "${entry}" \
    --arg version "${version}" \
    '. + [$entry + {version: $version}]' <<< "${RELEASES}")
done < <(jq -c 'to_entries[]
  | (.key | split("/")[:-1] | join("/")) as $base_dir
  | {
      dockerfile: .key,
      base_dir: $base_dir,
      project: ($base_dir | split("/") | last),
      frontend_manager: "none",
      frontend_dir: "-",
      embed_dir: "-"
    } + .value
  | . + {binary: (.binary // .project)}' "${TABLE}")

TEST_JSON=$(jq -c '[ .[] | {project, base_dir, embed_dir} ]' <<< "${RELEASES}")

BINARY_JSON=$(jq -c '[ .[] as $p
  | {"x86_64-unknown-linux-musl": "amd64", "aarch64-unknown-linux-musl": "arm64"}
  | to_entries[]
  | {
      project: $p.project,
      binary: $p.binary,
      base_dir: $p.base_dir,
      version: $p.version,
      target: .key,
      arch: .value,
      frontend_manager: $p.frontend_manager,
      frontend_dir: $p.frontend_dir,
      embed_dir: $p.embed_dir
    } ]' <<< "${RELEASES}")

DOCKER_JSON=$(jq -c '[ .[] | {
    project,
    binary,
    base_dir,
    image: ("younsl/" + .project),
    version,
    dockerfile
  } ]' <<< "${RELEASES}")

HAS_RELEASES="false"
[[ "$(jq 'length' <<< "${RELEASES}")" -gt 0 ]] && HAS_RELEASES="true"

{
  echo "has_releases=${HAS_RELEASES}"
  echo "test_matrix={\"include\":${TEST_JSON}}"
  echo "binary_matrix={\"include\":${BINARY_JSON}}"
  echo "docker_matrix={\"include\":${DOCKER_JSON}}"
} >> "${GITHUB_OUTPUT}"

echo "test_matrix: ${TEST_JSON}"
echo "binary_matrix: ${BINARY_JSON}"
echo "docker_matrix: ${DOCKER_JSON}"
