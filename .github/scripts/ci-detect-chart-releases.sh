#!/usr/bin/env bash
# Decide which Helm charts under box/kubernetes to release.
# Usage: ci-detect-chart-releases.sh   Writes has_changes and matrix to GITHUB_OUTPUT.
# Env: GH_TOKEN, GITHUB_ACTOR, GITHUB_SHA, GITHUB_OUTPUT, EVENT_NAME,
#   PUSH_BEFORE (push), INPUT_CHART and FORCE (workflow_dispatch).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INPUT_CHART="${INPUT_CHART:-}"
FORCE="${FORCE:-false}"

if [[ "${EVENT_NAME}" == "workflow_dispatch" ]]; then
  if [[ -n "${INPUT_CHART}" ]]; then
    CANDIDATES=$(find box/kubernetes -name Chart.yaml -path "*/${INPUT_CHART}/Chart.yaml" -not -path "*/.git/*" -exec dirname {} \; | head -1)
    if [[ -z "${CANDIDATES}" ]]; then
      echo "::error::Chart ${INPUT_CHART} not found"
      exit 1
    fi
  else
    CANDIDATES=$(find box/kubernetes -name Chart.yaml -not -path "*/.git/*" -exec dirname {} \; | sort)
  fi
else
  CANDIDATES=$("${SCRIPT_DIR}/ci-changed-files.sh" | grep '^box/kubernetes/.*/Chart\.yaml$' | xargs -r -n1 dirname || true)
fi

INCLUDES="[]"

while read -r chart_path; do
  [[ -n "${chart_path}" ]] || continue
  name=$(basename "${chart_path}")
  version=$(awk '/^version:/ {print $2; exit}' "${chart_path}/Chart.yaml")
  app_version=$(awk '/^appVersion:/ {gsub(/"/, "", $2); print $2; exit}' "${chart_path}/Chart.yaml")

  if [[ "$("${SCRIPT_DIR}/ci-ghcr-tag-status.sh" "younsl/charts/${name}" "${version}")" == "exists" \
        && "${FORCE}" != "true" ]]; then
    echo "Skip ${name}:${version} (already exists)"
    continue
  fi

  echo "Release ${name}:${version}"
  INCLUDES=$(jq -c \
    --arg n "${name}" --arg v "${version}" \
    --arg a "${app_version:-N/A}" --arg p "${chart_path}" \
    '. + [{chart_name: $n, version: $v, app_version: $a, chart_path: $p}]' <<< "${INCLUDES}")
done <<< "${CANDIDATES}"

COUNT=$(jq 'length' <<< "${INCLUDES}")
if [[ "${COUNT}" -eq 0 ]]; then
  {
    echo "has_changes=false"
    echo "matrix={}"
  } >> "${GITHUB_OUTPUT}"
else
  {
    echo "has_changes=true"
    echo "matrix=$(jq -nc --argjson i "${INCLUDES}" '{include: $i}')"
  } >> "${GITHUB_OUTPUT}"
  echo "Releasing ${COUNT} chart(s):"
  jq -r '.[] | "  - \(.chart_name):\(.version) (\(.chart_path))"' <<< "${INCLUDES}"
fi
