---
plugins:
  - opencost
  - opencost-backend
---

# OpenCost

Custom plugin for multi-cluster Kubernetes cost visualization with daily/monthly tracking and carbon emission enrichment.

## Prerequisites

1. **OpenCost installed**: Each target cluster must have [OpenCost](https://www.opencost.io/docs/installation/install) deployed (e.g., via [`opencost/opencost`](https://artifacthub.io/packages/helm/opencost/opencost) Helm chart).
2. **External access to opencost-ui**: The Backstage backend must be able to reach each cluster's OpenCost API endpoint. Expose the `opencost-ui` (port 9090) or `opencost` (port 9003) Service via HTTPRoute or Ingress so that Backstage can call the `/model/allocation` and `/model/assets/carbon` APIs.

```yaml
# HTTPRoute example
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: opencost
  namespace: opencost
spec:
  parentRefs:
    - name: internal-gateway
  hostnames:
    - opencost.example.com
  rules:
    - backendRefs:
        - name: opencost
          port: 9003
```

```yaml
# Ingress example
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: opencost
  namespace: opencost
spec:
  rules:
    - host: opencost.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: opencost
                port:
                  number: 9003
```

The exposed URL is what you set in `opencost.clusters[].url` (e.g., `https://opencost.example.com`).

## Features

- Multi-cluster cost dashboard with year/month/day drilldown navigation
- Per-pod daily cost breakdown (CPU, RAM, GPU, PV, Network, Total, Carbon)
- Monthly aggregation with automatic scheduled collection
- Carbon cost enrichment via OpenCost assets/carbon API
- Gap-fill validation for missing collection dates
- Collection run history for observability
- Controller-based filtering (Deployment, StatefulSet, DaemonSet, etc.)
- LRU cache with TTL-based expiration (5min current month, 24h past months)
- Timezone-aware billing boundary calculation

## Configuration

### Cluster Settings

```yaml
# app-config.yaml
opencost:
  timezone: Asia/Seoul
  clusters:
    - name: shared
      alias: Shared
      url: ${OPENCOST_SHARED_URL}
    - name: dev
      alias: Dev
      url: ${OPENCOST_DEV_URL}
    - name: stg
      alias: Stage
      url: ${OPENCOST_STG_URL}
    - name: prd
      alias: Production
      url: ${OPENCOST_PRD_URL}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timezone` | `string` | `UTC` | IANA timezone for billing day boundaries |
| `clusters[].name` | `string` | — | Cluster identifier (used in API calls) |
| `clusters[].alias` | `string` | — | Operator-facing cluster name shown in the UI and written to the Cluster column of CSV exports |
| `clusters[].url` | `string` | — | OpenCost API base URL |

### Controller Filter Presets

Presets are managed from the OpenCost page (Filters bar, Preset select, Manage button) and stored in the plugin database, not in app-config. A preset is a named list of SQL LIKE patterns matched against the controller (workload) name: `%` matches any run of characters and `_` exactly one. Patterns in a preset are ORed together. A preset can be limited to specific clusters, otherwise it is offered for all of them.

Selecting a preset restricts the yearly, monthly and daily views, the controller dropdown, the monthly Pod Breakdown table and every CSV export to matching pods, and the selection is carried in the URL as `filter=<name>` so a month-end billing view can be bookmarked per cluster. The editor previews the effect of the patterns against the selected cluster and month before saving.

Any authenticated Backstage user can manage presets. There is no finer-grained permission check.

### CSV Exports

Every CSV export starts with a `Cluster` column holding the cluster `alias` from `opencost.clusters`, so exports from several clusters can be concatenated without losing their origin.

### Sidebar Toggle

```yaml
# app-config.yaml
app:
  plugins:
    opencost: true  # Set to false to hide from sidebar
```

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `OPENCOST_SHARED_URL` | Per cluster | OpenCost API endpoint for shared cluster |
| `OPENCOST_DEV_URL` | Per cluster | OpenCost API endpoint for dev cluster |
| `OPENCOST_STG_URL` | Per cluster | OpenCost API endpoint for stage cluster |
| `OPENCOST_SB_URL` | Per cluster | OpenCost API endpoint for sandbox cluster |
| `OPENCOST_PRD_URL` | Per cluster | OpenCost API endpoint for production cluster |

Cluster entries with missing `url` are skipped during collection.

## Scheduled Tasks

Three scheduled tasks run automatically after plugin initialization:

| Task | Default Schedule | Timeout | Purpose |
|------|-----------------|---------|---------|
| `opencost:daily-collector` | 00:30 daily (in billing TZ) | 30 min | Collect yesterday's per-pod costs |
| `opencost:gap-validator` | Every hour | 30 min | Detect and backfill missing dates in current month |
| `opencost:monthly-aggregator` | 01:00 on 2nd (in billing TZ) | 30 min | Aggregate previous month's daily costs into summaries |

Cron expressions are automatically converted from the configured timezone to UTC before registration with the Backstage scheduler.

## Data Pipeline

```
OpenCost API ──► daily-collector ──► opencost_daily_costs (DB)
                                         │
                 gap-validator ──────────┘ (backfill missing dates)
                                         │
                 monthly-aggregator ─────► opencost_monthly_summaries (DB)
                                         │
                 REST API ◄──────────────┘
                   │
                 Frontend (drilldown views)
```

### Collection Flow

1. **Daily collector** runs at 00:30 in billing timezone
2. Calculates "yesterday" based on configured timezone
3. Calls OpenCost `/model/allocation` API (window = midnight to midnight UTC epoch)
4. Fetches carbon data from `/model/assets/carbon` API in parallel
5. Distributes carbon cost proportionally by pod's share of total cost
6. Upserts pod metadata to `opencost_pods`, inserts daily costs
7. Records collection run with status and pod count

### Carbon Cost Enrichment

Carbon cost (kg CO2e) is fetched from the OpenCost assets/carbon endpoint and distributed proportionally across pods:

```
pod_carbon = (pod_total_cost / cluster_total_cost) × cluster_total_carbon
```

## API Endpoints

Base path: `/api/opencost-backend/`

| Method | Path | Description |
|--------|------|-------------|
| GET | `/health` | Health check |
| GET | `/config` | Timezone and cron schedule |
| GET | `/clusters/status` | Cluster connectivity status |
| GET | `/allocation` | Proxy to OpenCost API with carbon enrichment |
| GET | `/costs/years` | Available years for cluster |
| GET | `/costs/controllers` | Server-side controller search for the filter dropdown: substring `q`, `kinds`, `excludeKinds`, `limit` (default 50, max 500), ordered by total cost, `truncated` flag. Whole year when `month` is omitted |
| GET | `/costs/monthly-totals` | One aggregated row per month, used by the yearly overview |
| GET | `/costs/daily-summary` | Per-day aggregates for month |
| GET | `/costs` | Monthly pod costs (summary or real-time aggregation) |
| GET | `/costs/daily` | Daily costs for specific pod |
| GET | `/costs/pods` | All pod costs for specific date |
| GET | `/costs/collection-runs` | Task execution history |
| GET | `/filters` | Controller filter presets |
| PUT | `/filters/{name}` | Create or replace a preset (`title`, `description`, `patterns[]`, `clusters[]`). Records the calling user as `createdBy` or `updatedBy` |
| DELETE | `/filters/{name}` | Delete a preset |
| POST | `/filters/preview` | Dry-run `patterns[]` against a `cluster`, `year`, `month` and return matched controllers, pod count, total and sample pods |

### Query Parameters

| Endpoint | Parameter | Required | Description |
|----------|-----------|----------|-------------|
| `/costs/*` | `cluster` | Yes | Cluster name |
| `/costs/*` | `year` | Yes | Year (YYYY) |
| `/costs/*` | `month` | Yes, optional for `/costs/controllers` | Month (1-12) |
| `/costs`, `/costs/monthly-totals`, `/costs/daily-summary`, `/costs/controllers`, `/costs/pods` | `controllers` | No | Comma-separated exact controller names |
| `/costs`, `/costs/monthly-totals`, `/costs/daily-summary`, `/costs/controllers`, `/costs/pods` | `filter` | No | Preset name. Its LIKE patterns are ORed together and ANDed with `controllers`. 400 if unknown or not enabled for the cluster |
| `/costs/daily` | `pod` | Yes | Pod name |
| `/costs/pods` | `date` | Yes | Date (YYYY-MM-DD) |

## Database

Six tables with 3NF-normalized schema. See [OpenCost ERD](erd.md) for full schema details.

| Table | Purpose |
|-------|---------|
| `opencost_meta` | Schema version tracking |
| `opencost_clusters` | Registered clusters |
| `opencost_pods` | Pod dimension (namespace, controller metadata) |
| `opencost_daily_costs` | Per-pod daily cost snapshots |
| `opencost_monthly_summaries` | Aggregated monthly costs per pod |
| `opencost_controller_filters` | Admin-defined controller filter presets (LIKE patterns as JSON text) |
| `opencost_collection_runs` | Task execution history |

## Cache Strategy

In-memory LRU cache (max 50 entries) with differentiated TTL:

| Data | TTL | Reason |
|------|-----|--------|
| Current month | 5 min | Data changes with ongoing collection |
| Past months | 24 hours | Data is finalized |

## Helm Chart Configuration

```yaml
backstage:
  extraEnvVars:
    - name: OPENCOST_SHARED_URL
      value: "http://opencost.opencost:9003"
    - name: OPENCOST_DEV_URL
      value: "http://opencost.opencost:9003"
    - name: OPENCOST_PRD_URL
      value: "http://opencost.opencost:9003"
```

## Troubleshooting

| Issue | Cause | Solution |
|-------|-------|----------|
| No cost data for yesterday | Daily collector hasn't run yet | Wait until 00:30 in billing timezone, or check collection runs |
| Missing dates in month view | Collection gap | Gap validator runs hourly to backfill automatically |
| Carbon cost shows 0 | OpenCost carbon API unavailable | Verify OpenCost has carbon data enabled |
| Cluster shows unhealthy | OpenCost API unreachable | Check `url` in config and network connectivity |
