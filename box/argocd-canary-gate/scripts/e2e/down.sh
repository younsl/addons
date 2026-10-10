#!/usr/bin/env bash
set -euo pipefail

# Never let a stray KUBECONFIG from the caller point this at anything real.
unset KUBECONFIG

CLUSTER="${CLUSTER:-acg-ui}"
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
unset KUBECONFIG

# `kind get clusters` is broken with podman 5.8, so the node container is the
# thing to look for. See the note in common.sh.
if "${BUILDER}" ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "${CLUSTER}-control-plane"; then
  KUBECONFIG="${KUBECONFIG_FILE}" kind delete cluster --name "${CLUSTER}"
fi
rm -f "${KUBECONFIG_FILE}"
