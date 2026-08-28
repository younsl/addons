# Prometheus Metrics

trivy-collector exposes Prometheus metrics in OpenMetrics format at the health server's `/metrics` endpoint (default port `8080`).

Metrics are mode-specific, and follow ownership rather than convenience: the scraper owns the database, so the database gauges live there, while the server owns request handling and the authored-state caches. Registering a metric a pod can never move would publish a permanent zero.

**Target audience**: Platform Engineers and SREs configuring monitoring and alerting for trivy-collector.

## Endpoint

| Path | Port | Format |
|------|------|--------|
| `/metrics` | `8080` (health port) | OpenMetrics text |

The `/metrics` endpoint shares the same health server as `/healthz` and `/readyz`. No separate port is required, and the scraper's internal API port (`8081`) is never scraped.

## Common Metrics

Available in both modes.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_info` | Gauge | `version`, `mode` | Build information (always 1) |

## Server Mode Metrics

### HTTP

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_http_requests_total` | Counter | `method`, `status` | Total HTTP requests |
| `trivy_collector_http_request_duration_seconds` | Histogram | `method` | HTTP request duration |

Histogram buckets: `0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0`

Excluded paths (not counted): `/healthz`, `/readyz`, `/metrics`, `/assets/*`, `/static/*`

### Reports

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_reports_received_total` | Counter | `cluster`, `report_type` | Reports accepted on the push ingest route and forwarded to the scraper |

### Authored state

Both are derived from watch caches, so they are in-memory reads refreshed every **60 seconds**.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_notes_configmap_bytes` | Gauge | — | Serialized size of the notes ConfigMap |
| `trivy_collector_api_tokens` | Gauge | — | API tokens held in the tokens Secret |

A ConfigMap caps at roughly 1MiB and the write path rejects anything past 800KiB, so the notes gauge is the headroom warning. Alert on it rather than discovering the wall.

### MCP

Registered whether or not `/mcp` is mounted.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_mcp_tool_calls_total` | Counter | `tool`, `result` | MCP tool invocations by outcome |
| `trivy_collector_mcp_tool_duration_seconds` | Histogram | `tool` | Tool execution time, including queueing for a concurrency slot |
| `trivy_collector_mcp_tool_calls_in_flight` | Gauge | — | Tool invocations currently executing |

## Scraper Mode Metrics

### Database

Refreshed every **60 seconds** by a background task.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_db_size_bytes` | Gauge | — | SQLite file size on the scraper's `emptyDir` |
| `trivy_collector_db_reports` | Gauge | `report_type` | Reports currently mirrored into the database |

The database starts empty on every restart and is rebuilt from the clusters that own the reports, so `trivy_collector_db_size_bytes` sawtooths across restarts by design. Watch it against the volume's `sizeLimit` rather than as a growth trend.

### Hydration

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `trivy_collector_clusters` | Gauge | — | Clusters registered with the scraper |
| `trivy_collector_fleet_hydrated` | Gauge | — | `1` once every registered cluster has finished its initial sync |

`trivy_collector_fleet_hydrated == 0` is expected for the first minutes after a restart, and `/readyz` fails for exactly that window. Sustained `0` means a cluster is unreachable and its slice of the data is missing.

## ServiceMonitor

Enable Prometheus Operator scraping per component:

```yaml
server:
  serviceMonitor:
    enabled: true
    interval: 30s
scraper:
  serviceMonitor:
    enabled: true
    interval: 30s
```

The chart creates a Service per component with a `metrics` port (8080) targeting the health server, and a ServiceMonitor pointing at `port: metrics`, `path: /metrics`. The scraper Service also carries the `internal` port, which no ServiceMonitor selects.

Requires the `monitoring.coreos.com/v1` API (Prometheus Operator CRDs) in the cluster.

## No Data Prevention

Counters and per-type gauges are pre-initialized with zeros at startup so the time series exist from the first scrape. This prevents "No data" in Grafana when no events have occurred yet.

## Example PromQL

```promql
# HTTP error rate
sum(rate(trivy_collector_http_requests_total{status=~"5.."}[5m]))
/ sum(rate(trivy_collector_http_requests_total[5m]))

# Fleet has been unhydrated for more than 10 minutes: a cluster is unreachable
min_over_time(trivy_collector_fleet_hydrated[10m]) == 0

# Database against the emptyDir sizeLimit (2Gi by default)
trivy_collector_db_size_bytes / (2 * 1024 * 1024 * 1024)

# Notes ConfigMap approaching the write-path ceiling (800KiB)
trivy_collector_notes_configmap_bytes / (800 * 1024) > 0.8

# MCP tool error rate
sum(rate(trivy_collector_mcp_tool_calls_total{result!="success"}[5m]))
/ sum(rate(trivy_collector_mcp_tool_calls_total[5m]))
```
