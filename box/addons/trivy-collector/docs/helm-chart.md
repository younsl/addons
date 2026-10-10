# Helm Chart

This document provides Helm chart installation and configuration guide for trivy-collector. Helm chart is the officially recommended installation method for trivy-collector.

**Target audience**: Platform Engineers and DevOps Engineers deploying trivy-collector to Kubernetes clusters.

## Values Reference

```yaml
# Deployment mode
mode: collector  # or "server"

# Collector settings
collector:
  serverUrl: "http://trivy-server:3000"
  clusterName: "my-cluster"
  namespaces: []  # empty = all namespaces
  collectVulnerabilityReports: true
  collectSbomReports: true

# Server settings
server:
  port: 3000
  replicaCount: 1
  gateway:  # Gateway API HTTPRoute, the only supported ingress path
    enabled: false
    parentRefs:
      - name: main-gateway
        namespace: gateway-system

# Internal API between the scraper (which owns the database) and the server
internal:
  port: 8081
  token: ""          # generated on first install when empty
  networkPolicy:
    enabled: true

# Common settings
health:
  port: 8080

logging:
  format: json
  level: info

resources:
  limits:
    memory: 256Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

## Installation Examples

### Server exposed through Gateway API

```bash
helm install trivy-server ./charts/trivy-collector \
  --namespace trivy-system \
  --set server.gateway.enabled=true \
  --set server.gateway.hostnames[0]=trivy.example.com
```

The chart renders an `HTTPRoute` only. `Ingress` support was removed, so an
`Ingress` in front of this release has to be authored outside the chart (for
example through `extraObjects`).

The release creates no PersistentVolumeClaim. The scraper keeps SQLite on its own `emptyDir` and rebuilds it from the watched clusters on restart; report notes and API tokens live in a ConfigMap and a Secret.

### Migrating off an existing PersistentVolume

API tokens are hashed and report notes are typed by a human, so both are unrecoverable and must be exported before the volume goes away. Reports need no export.

```bash
helm upgrade trivy-collector ./charts/trivy-collector \
  --namespace trivy-system \
  --set migration.exportState.enabled=true \
  --set migration.exportState.existingClaim=trivy-collector
```

The hook Job mounts the old PVC read-only and writes `{release}-api-tokens` and `{release}-notes`. Verify both objects, confirm hydration completes and that a known token still authenticates and a known note still renders, then disable the hook and delete the PVC. Until that last step the PVC is the rollback.

### Collector watching specific namespaces

```bash
helm install trivy-collector ./charts/trivy-collector \
  --namespace trivy-system \
  --set mode=collector \
  --set collector.serverUrl=http://trivy-server:3000 \
  --set collector.clusterName=production \
  --set collector.namespaces="{default,kube-system,app}"
```

### Server with Gateway API HTTPRoute

```bash
helm install trivy-server ./charts/trivy-collector \
  --namespace trivy-system \
  --set mode=server \
  --set server.gateway.enabled=true \
  --set server.gateway.hostnames[0]=trivy.example.com
```
