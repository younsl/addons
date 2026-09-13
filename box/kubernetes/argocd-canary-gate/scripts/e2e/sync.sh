#!/usr/bin/env bash
# Ask Argo CD to sync an Application by writing its operation field, which is
# the same write the UI Sync button and the argocd CLI make and exactly what
# the gate's webhook intercepts. A denial surfaces as the kubectl error, a
# warn-mode verdict as a kubectl Warning line.
set -euo pipefail

CLUSTER="${CLUSTER:-acg-ui}"
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

APP="${1:?usage: sync.sh <application>}"

# An operation already on the object means a sync is still running, and a
# second write would look like a retry rather than a new sync to the gate.
pending=$("${KUBECTL[@]}" -n argocd get application "${APP}" \
  -o jsonpath='{.operation}' 2>/dev/null || true)
if [[ -n "${pending}" ]]; then
  echo "a sync of ${APP} is already in flight, wait for it to finish and retry" >&2
  exit 1
fi

"${KUBECTL[@]}" -n argocd patch application "${APP}" --type merge \
  -p '{"operation":{"initiatedBy":{"username":"local-e2e"},"sync":{"prune":false}}}'

echo "sync accepted. watch it with: kubectl --context ${CTX} -n argocd get application ${APP} -w"
