#!/usr/bin/env bash
# Print "exists" or "new" for a tag on GHCR. Exits non-zero if the lookup fails,
# so a caller never reads an unreadable registry as "not published".
# Usage: ci-ghcr-tag-status.sh <image> <tag>   Env: GH_TOKEN, GITHUB_ACTOR
set -euo pipefail

IMAGE="${1:?Usage: ci-ghcr-tag-status.sh <image> <tag>}"
TAG="${2:?}"
REGISTRY="${REGISTRY:-ghcr.io}"

TOKEN=$(curl -sS -u "${GITHUB_ACTOR}:${GH_TOKEN}" \
  "https://${REGISTRY}/token?service=${REGISTRY}&scope=repository:${IMAGE}:pull" \
  | jq -r '.token // empty')
if [[ -z "${TOKEN}" ]]; then
  echo "::error::GHCR refused a pull token for ${IMAGE}" >&2
  exit 1
fi

BODY=$(mktemp)
HTTP=$(curl -sS -o "${BODY}" -w '%{http_code}' \
  -H "Authorization: Bearer ${TOKEN}" \
  "https://${REGISTRY}/v2/${IMAGE}/tags/list")

case "${HTTP}" in
  200)
    if jq -e --arg v "${TAG}" '(.tags // []) | index($v)' "${BODY}" > /dev/null; then
      echo "exists"
    else
      echo "new"
    fi
    ;;
  404) echo "new" ;;
  *)
    echo "::error::GHCR lookup for ${IMAGE} failed (HTTP ${HTTP})" >&2
    exit 1
    ;;
esac
