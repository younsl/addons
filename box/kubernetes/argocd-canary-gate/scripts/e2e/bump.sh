#!/usr/bin/env bash
# Commit a new nginx tag for the Rollout into the in-cluster Gitea repo, the
# way a real deploy changes desired state: through git, not kubectl. The app
# goes OutOfSync and the next sync starts the canary.
set -euo pipefail

CLUSTER="${CLUSTER:-acg-ui}"
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

TAG="${1:?usage: bump.sh <nginx tag, e.g. 1.28-alpine>}"

gitea_api() {
  "${KUBECTL[@]}" -n git exec deploy/gitea -- \
    curl -sf -u "${GITEA_USER}:${GITEA_PASSWORD}" "$@"
}

current=$(gitea_api "http://localhost:3000/api/v1/repos/${GITEA_USER}/canary-demo/contents/rollout.yaml?ref=main")
sha=$(printf '%s' "${current}" | python3 -c "import sys,json; print(json.load(sys.stdin)['sha'])")
content=$(printf '%s' "${current}" | python3 -c "
import base64, json, re, sys
doc = base64.b64decode(json.load(sys.stdin)['content']).decode()
doc = re.sub(r'image: docker\.io/library/nginx:\S+', 'image: docker.io/library/nginx:${TAG}', doc)
print(base64.b64encode(doc.encode()).decode())
")

gitea_api -X PUT "http://localhost:3000/api/v1/repos/${GITEA_USER}/canary-demo/contents/rollout.yaml" \
  -H 'Content-Type: application/json' \
  -d "{\"content\":\"${content}\",\"message\":\"deploy nginx ${TAG}\",\"branch\":\"main\",\"sha\":\"${sha}\"}" \
  -o /dev/null

"${KUBECTL[@]}" -n argocd annotate application canary-demo argocd.argoproj.io/refresh=normal --overwrite >/dev/null
echo "committed nginx ${TAG}. the app will show OutOfSync in a few seconds; sync it to start the canary"
