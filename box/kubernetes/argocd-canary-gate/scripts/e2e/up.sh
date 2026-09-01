#!/usr/bin/env bash
# Stand up a local cluster with Argo CD, Argo Rollouts, an in-cluster Gitea,
# and the gate, for pressing Sync by hand and watching the gate allow or deny
# it.
#
# Everything is real: the Rollout status the gate judges is written by the
# actual Argo Rollouts controller, the sync the gate intercepts is the actual
# Application operation write, and the Application's source is a real git repo
# (Gitea) seeded with rollout.yaml, so the Rollout renders in the Argo CD
# resource tree and a canary starts the production way: a commit bumps the
# image, a sync applies it.
set -euo pipefail

CLUSTER="${CLUSTER:-acg-ui}"
ARGOCD_CHART_VERSION="${ARGOCD_CHART_VERSION:-10.2.1}"        # appVersion v3.4.5
ROLLOUTS_CHART_VERSION="${ROLLOUTS_CHART_VERSION:-2.42.0}"    # appVersion v1.9.1
DEMO_NAMESPACE="canary-gate-test"
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

log "cluster ${CLUSTER}"
create_cluster
build_and_load_image

log "installing Argo CD ${ARGOCD_CHART_VERSION}"
helm repo add argo https://argoproj.github.io/argo-helm >/dev/null 2>&1 || true
helm repo update argo >/dev/null
"${KUBECTL[@]}" create namespace argocd --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
"${HELM[@]}" upgrade --install argocd argo/argo-cd \
  --version "${ARGOCD_CHART_VERSION}" \
  --namespace argocd \
  --values "${E2E_DIR}/argocd-values.yaml" \
  --wait --timeout 600s

log "installing Argo Rollouts ${ROLLOUTS_CHART_VERSION}"
"${HELM[@]}" upgrade --install argo-rollouts argo/argo-rollouts \
  --version "${ROLLOUTS_CHART_VERSION}" \
  --namespace argo-rollouts --create-namespace \
  --set dashboard.enabled=false \
  --wait --timeout 300s

log "installing the gate"
"${HELM[@]}" upgrade --install argocd-canary-gate \
  "${REPO_ROOT}/charts/argocd-canary-gate" \
  --namespace argocd \
  --values "${E2E_DIR}/values.yaml" \
  --wait --timeout 180s

log "installing Gitea and seeding the demo repo"
"${KUBECTL[@]}" apply -f "${E2E_DIR}/gitea.yaml"
"${KUBECTL[@]}" -n git rollout status deploy/gitea --timeout=300s
# Reruns are fine: the user and the repo already existing are not errors, and
# the contents POST is skipped when rollout.yaml is already committed.
"${KUBECTL[@]}" -n git exec deploy/gitea -- su git -c \
  "gitea admin user create --admin --username ${GITEA_USER} --password ${GITEA_PASSWORD} --email ${GITEA_USER}@example.com --must-change-password=false" \
  2>/dev/null || echo "gitea user ${GITEA_USER} already exists"
"${KUBECTL[@]}" -n git exec deploy/gitea -- curl -sf -u "${GITEA_USER}:${GITEA_PASSWORD}" \
  -X POST http://localhost:3000/api/v1/user/repos -H 'Content-Type: application/json' \
  -d '{"name":"canary-demo","auto_init":true,"private":false,"default_branch":"main"}' \
  -o /dev/null || echo "repo canary-demo already exists"
if ! "${KUBECTL[@]}" -n git exec deploy/gitea -- curl -sf -o /dev/null \
  "http://localhost:3000/${GITEA_USER}/canary-demo/raw/branch/main/rollout.yaml"; then
  ROLLOUT_B64=$(base64 < "${E2E_DIR}/rollout.yaml" | tr -d '\n')
  "${KUBECTL[@]}" -n git exec deploy/gitea -- curl -sf -u "${GITEA_USER}:${GITEA_PASSWORD}" \
    -X POST "http://localhost:3000/api/v1/repos/${GITEA_USER}/canary-demo/contents/rollout.yaml" \
    -H 'Content-Type: application/json' \
    -d "{\"content\":\"${ROLLOUT_B64}\",\"message\":\"add canary rollout\",\"branch\":\"main\"}" \
    -o /dev/null
fi

log "fixtures"
"${KUBECTL[@]}" create namespace "${DEMO_NAMESPACE}" --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" apply -f "${E2E_DIR}/application.yaml"

log "first sync: deploys the Rollout from git"
# Allowed by the gate, since the app manages no Rollout yet. A first deploy
# promotes without running the steps, so it settles at Healthy, which is the
# state the walkthrough starts from.
sleep 5
"${E2E_DIR}/sync.sh" canary-demo
for _ in $(seq 1 36); do
  phase=$("${KUBECTL[@]}" -n "${DEMO_NAMESPACE}" get rollout canary-demo-web \
    -o jsonpath='{.status.phase}' 2>/dev/null || true)
  [[ "${phase}" == "Healthy" ]] && break
  sleep 5
done

log "state"
"${KUBECTL[@]}" -n "${DEMO_NAMESPACE}" get rollout canary-demo-web
"${KUBECTL[@]}" -n argocd get applications
"${KUBECTL[@]}" -n argocd get pods -l app.kubernetes.io/name=argocd-canary-gate

password=$("${KUBECTL[@]}" -n argocd get secret argocd-initial-admin-secret \
  -o jsonpath='{.data.password}' 2>/dev/null | base64 -d || echo '<not created>')

cat <<EOF

=== log in

  kubectl --context ${CTX} -n argocd port-forward svc/argocd-server 8080:80

  http://localhost:8080
  user: admin
  pass: ${password}

Open the canary-demo application. Its tree shows the Rollout, its ReplicaSets,
and its pods, because the Rollout comes from the app's git source (the
in-cluster Gitea repo).

=== 1. start a canary the production way: commit, then sync

  ${E2E_DIR}/bump.sh 1.28-alpine

The app goes OutOfSync. Press SYNC in the UI (or run ${E2E_DIR}/sync.sh
canary-demo). The gate allows it: nothing was mid-flight. Argo Rollouts starts
the canary and pauses at step 2/4, which the tree shows as a Suspended Rollout
with one canary pod.

=== 2. failure: mid-canary sync is denied

Press SYNC again while the Rollout is paused. The denial arrives as a red
toast, naming the Rollout and its step position. From the CLI the same thing:

  ${E2E_DIR}/sync.sh canary-demo

It is also recorded on the Application:

  kubectl --context ${CTX} -n argocd describe application canary-demo | tail -n 8

=== 3. bypass: the skip annotation

  kubectl --context ${CTX} -n argocd annotate application canary-demo \\
    canary-gate.younsl.github.io/skip=true
  ${E2E_DIR}/sync.sh canary-demo          # allowed despite the canary
  kubectl --context ${CTX} -n argocd annotate application canary-demo \\
    canary-gate.younsl.github.io/skip-

=== 4. promote, then sync goes through again

  # brew install argoproj/tap/kubectl-argo-rollouts
  kubectl argo rollouts --context ${CTX} -n ${DEMO_NAMESPACE} promote canary-demo-web
  # or abort back to stable instead: ... abort canary-demo-web
  # then, once the Rollout is Healthy:
  ${E2E_DIR}/sync.sh canary-demo

=== 5. warn mode: report instead of deny

  helm --kube-context ${CTX} upgrade argocd-canary-gate \\
    ${REPO_ROOT}/charts/argocd-canary-gate -n argocd \\
    -f ${E2E_DIR}/values.yaml --set canaryGate.mode=warn

Start another canary (step 1) and sync: kubectl prints the gate's message as a
Warning and the sync proceeds.

=== observability

  kubectl --context ${CTX} -n argocd logs deploy/argocd-canary-gate -f
  kubectl --context ${CTX} -n argocd port-forward svc/argocd-canary-gate 9090:8080
  curl -s localhost:9090/metrics | grep argocd_canary_gate_decisions

=== tear down

  ${E2E_DIR}/down.sh
EOF
print_kubeconfig_hint
