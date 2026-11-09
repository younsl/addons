# Usage

This guide covers running the gateway locally against a port-forwarded kagent controller, replaying an alert by hand, and installing the Helm chart. It is for developers testing a change and operators installing the gateway.

## Local run

```bash
# Local run against a port-forwarded controller
kubectl -n kagent port-forward svc/kagent-controller 8083:8083

SLACK_BOT_TOKEN=xoxb-... SLACK_DEFAULT_CHANNEL=alerts-test \
KAGENT_URL=http://localhost:8083 LOG_FORMAT=text make run

# Replay an alert
curl -X POST localhost:8080/alert -H 'Content-Type: application/json' -d '{
  "groupKey": "test-1", "status": "firing", "receiver": "kagent-gateway",
  "commonLabels": {"alertname": "KubePodCrashLooping", "severity": "critical", "cluster": "prd"},
  "alerts": [{"status": "firing", "fingerprint": "fp-1",
    "labels": {"alertname": "KubePodCrashLooping", "severity": "critical", "namespace": "demo", "pod": "demo-api-0"},
    "annotations": {"summary": "pod is restarting", "description": "7 restarts in 10 minutes"}}]
}'
```

## Helm

```bash
helm install kagent-gateway ./charts/kagent-gateway \
  --namespace kagent \
  --set slack.parentMode=lookup \
  --set slack.defaultChannel=alerts-test \
  --set slack.existingSecret=kagent-gateway-slack \
  --set kagent.agent=alert-triage-agent \
  --set serviceMonitor.enabled=true
```

The chart is also released to the OCI registry on `Chart.yaml` version bumps. List the published versions with crane:

```bash
crane ls ghcr.io/younsl/charts/kagent-gateway
helm install kagent-gateway oci://ghcr.io/younsl/charts/kagent-gateway --version 0.4.0
```

Values are documented in the [chart README](../charts/kagent-gateway/README.md).

## Conclusion

A local run needs only a bot token, a default channel, and a port-forwarded controller, and a curl to `/alert` replays any Alertmanager payload without Alertmanager. In a cluster, install the OCI chart with an existing Slack Secret and the agent name.
