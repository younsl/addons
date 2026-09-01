#!/usr/bin/env bash
# Stand up a local cluster with Argo CD, Argo Rollouts, and the gate, for
# pressing Sync by hand and watching the gate allow or deny it.
#
# Both controllers are real: the Rollout status the gate judges is written by
# the actual Argo Rollouts controller, and the sync the gate intercepts is the
# actual Application operation write. Nothing is stubbed.
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

log "fixtures"
"${KUBECTL[@]}" create namespace "${DEMO_NAMESPACE}" --dry-run=client -o yaml | "${KUBECTL[@]}" apply -f -
"${KUBECTL[@]}" apply -f "${E2E_DIR}/rollout.yaml"
"${KUBECTL[@]}" apply -f "${E2E_DIR}/application.yaml"

# A first deploy promotes without running the steps, so the Rollout settles at
# Healthy with stable == current. That is the state the success test needs.
log "waiting for the Rollout's first deploy to settle"
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

=== log in (optional, sync.sh works without the UI)

  kubectl --context ${CTX} -n argocd port-forward svc/argocd-server 8080:80

  http://localhost:8080
  user: admin
  pass: ${password}

=== 1. success: settled Rollout, sync allowed

  ${E2E_DIR}/sync.sh canary-demo

The Rollout's stable and current pod hashes agree, so the gate passes the sync
and Argo CD deploys podinfo into ${DEMO_NAMESPACE}. Give the controller a few
seconds to finish before the next step.

=== 2. start a canary and leave it mid-flight

  kubectl --context ${CTX} -n ${DEMO_NAMESPACE} patch rollout canary-demo-web \\
    --type json -p '[{"op":"replace","path":"/spec/template/spec/containers/0/image","value":"docker.io/library/nginx:1.28-alpine"}]'

  # wait until it pauses at step 2/4
  kubectl --context ${CTX} -n ${DEMO_NAMESPACE} get rollout canary-demo-web -w

=== 3. failure: mid-canary sync denied

  ${E2E_DIR}/sync.sh canary-demo

kubectl exits with the admission denial, naming the Rollout and its step
position. The same denial arrives as a red toast if you press SYNC in the UI,
and it is recorded on the Application:

  kubectl --context ${CTX} -n argocd describe application canary-demo | tail -n 8

=== 4. bypass: the skip annotation

  kubectl --context ${CTX} -n argocd annotate application canary-demo \\
    canary-gate.younsl.github.io/skip=true
  ${E2E_DIR}/sync.sh canary-demo          # allowed despite the canary
  kubectl --context ${CTX} -n argocd annotate application canary-demo \\
    canary-gate.younsl.github.io/skip-

=== 5. promote, then sync goes through again

  # brew install argoproj/tap/kubectl-argo-rollouts
  kubectl argo rollouts --context ${CTX} -n ${DEMO_NAMESPACE} promote canary-demo-web
  # or abort back to stable instead: ... abort canary-demo-web
  # then, once Healthy:
  ${E2E_DIR}/sync.sh canary-demo

=== 6. warn mode: report instead of deny

  helm --kube-context ${CTX} upgrade argocd-canary-gate \\
    ${REPO_ROOT}/charts/argocd-canary-gate -n argocd \\
    -f ${E2E_DIR}/values.yaml --set canaryGate.mode=warn

Start another canary (step 2) and sync: kubectl prints the gate's message as a
Warning and the sync proceeds.

=== observability

  kubectl --context ${CTX} -n argocd logs deploy/argocd-canary-gate -f
  kubectl --context ${CTX} -n argocd port-forward svc/argocd-canary-gate 9090:8080
  curl -s localhost:9090/metrics | grep argocd_canary_gate_decisions

=== tear down

  ${E2E_DIR}/down.sh
EOF
print_kubeconfig_hint
