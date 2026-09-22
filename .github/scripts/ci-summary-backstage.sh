#!/usr/bin/env bash
# Generate GitHub Actions Job Summary for backstage image release.
# Usage: summary-backstage.sh <image> <version> <commit>
set -euo pipefail

IMAGE="${1:?Usage: summary-backstage.sh <image> <version> <commit>}"
VERSION="${2:?}"
COMMIT="${3:?}"

IMAGE_SIZE=$(docker image inspect "${IMAGE}" --format='{{.Size}}' 2>/dev/null || echo "0")
IMAGE_SIZE_MB=$(awk "BEGIN {printf \"%.1f\", ${IMAGE_SIZE} / 1024 / 1024}")

# Read from the manifest list, not the pulled image: docker pull resolves to the
# runner's own architecture, so a silent drop back to a single-arch image would
# otherwise look identical here.
PLATFORMS=$(docker buildx imagetools inspect "${IMAGE}" --format '{{range .Manifest.Manifests}}{{if ne .Platform.OS "unknown"}}{{.Platform.OS}}/{{.Platform.Architecture}} {{end}}{{end}}' 2>/dev/null | tr -s ' ' | sed 's/ $//' || echo "unknown")
[ -n "${PLATFORMS}" ] || PLATFORMS="unknown"

cat <<EOF >> "$GITHUB_STEP_SUMMARY"
## Backstage Image Released

| Item | Value |
|------|-------|
| Image | \`${IMAGE}\` |
| Backstage Version | ${VERSION} |
| Commit | ${COMMIT} |
| Image Size | ${IMAGE_SIZE_MB} MB |
| Platforms | ${PLATFORMS} |
EOF
