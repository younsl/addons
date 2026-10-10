---
plugins:
  - opencost
  - opencost-backend
---

# OpenCost ERD

Schema version: **2**

## ER Diagram

```mermaid
erDiagram
    opencost_meta {
        varchar(100) key PK
        text value
    }

    opencost_clusters {
        integer id PK
        varchar(100) name UK
        varchar(100) title
        datetime created_at
        datetime updated_at
    }

    opencost_pods {
        integer id PK
        integer cluster_id FK
        varchar(253) namespace
        varchar(50) controller_kind "nullable"
        varchar(253) controller "nullable"
        varchar(253) pod
        datetime created_at
        datetime updated_at
    }

    opencost_daily_costs {
        integer id PK
        integer cluster_id FK
        date date
        integer pod_id FK
        decimal(12_4) cpu_cost
        decimal(12_4) ram_cost
        decimal(12_4) gpu_cost
        decimal(12_4) pv_cost
        decimal(12_4) network_cost
        decimal(12_4) total_cost
        decimal(12_4) carbon_cost
        datetime created_at
        datetime updated_at
    }

    opencost_monthly_summaries {
        integer id PK
        integer cluster_id FK
        smallint year
        smallint month
        integer pod_id FK
        decimal(12_4) cpu_cost
        decimal(12_4) ram_cost
        decimal(12_4) gpu_cost
        decimal(12_4) pv_cost
        decimal(12_4) network_cost
        decimal(12_4) total_cost
        decimal(12_4) carbon_cost
        smallint days_covered
        datetime created_at
        datetime updated_at
    }

    opencost_collection_runs {
        integer id PK
        integer cluster_id FK
        varchar(20) task_type "daily | gap-fill | monthly-agg"
        date target_date "nullable"
        smallint target_year "nullable"
        smallint target_month "nullable"
        varchar(20) status "success | failure | partial"
        integer pods_collected
        text error_message "nullable"
        datetime started_at
        datetime finished_at "nullable"
    }

    opencost_controller_filters {
        integer id PK
        varchar(64) name UK "slug, used as ?filter="
        varchar(100) title
        text description "nullable"
        text patterns "JSON array of SQL LIKE patterns"
        text clusters "nullable JSON array of cluster names"
        varchar(253) created_by "nullable user entity ref"
        varchar(253) updated_by "nullable user entity ref"
        datetime created_at
        datetime updated_at
    }

    opencost_clusters ||--o{ opencost_pods : "has"
    opencost_clusters ||--o{ opencost_daily_costs : "has"
    opencost_clusters ||--o{ opencost_monthly_summaries : "has"
    opencost_clusters ||--o{ opencost_collection_runs : "has"
    opencost_pods ||--o{ opencost_daily_costs : "has"
    opencost_pods ||--o{ opencost_monthly_summaries : "has"
```

## Tables

### opencost_meta

Schema version tracking. Single row with `key = 'schema_version'`.

### opencost_clusters

Registered OpenCost clusters.

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| UNIQUE | `name` |

### opencost_pods

Pod dimension table (3NF normalized). Stores pod identity and mutable controller metadata.

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| UNIQUE | `(cluster_id, namespace, pod)` |
| FK | `cluster_id` → `opencost_clusters.id` |
| INDEX | `cluster_id` |

`controller_kind` and `controller` are updated on upsert when they change; `updated_at` tracks the last change.

### opencost_daily_costs

Per-pod daily cost snapshot. One row per (cluster, date, pod).

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| UNIQUE | `(cluster_id, date, pod_id)` |
| FK | `cluster_id` → `opencost_clusters.id` |
| FK | `pod_id` → `opencost_pods.id` |
| INDEX | `(cluster_id, date)` |

On upsert (re-collection or gap-fill), `created_at` is preserved and only `updated_at` is refreshed.

### opencost_monthly_summaries

Aggregated monthly cost per pod. Produced by the monthly-aggregator scheduled task.

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| UNIQUE | `(cluster_id, year, month, pod_id)` |
| FK | `cluster_id` → `opencost_clusters.id` |
| FK | `pod_id` → `opencost_pods.id` |
| INDEX | `(cluster_id, year, month)` |

### opencost_collection_runs

Snapshot execution history for observability.

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| FK | `cluster_id` → `opencost_clusters.id` |
| INDEX | `cluster_id` |
| INDEX | `(task_type, status)` |

| task_type | target fields used |
|-----------|--------------------|
| `daily` | `target_date` |
| `gap-fill` | `target_date` |
| `monthly-agg` | `target_year`, `target_month` |

Lifecycle: row inserted with `status = 'partial'` at start, updated to `success` or `failure` on completion.

### opencost_controller_filters

Admin-defined controller filter presets, managed from the UI. Not related to any cluster row: `clusters` is an optional JSON list of cluster names that limits where the preset is offered, and `null` means every cluster.

| Constraint | Columns |
|------------|---------|
| PK | `id` |
| UNIQUE | `name` |

`patterns` and `clusters` are JSON arrays serialised as `text` so the column type is identical on SQLite and PostgreSQL. Patterns are SQL LIKE expressions applied to `opencost_pods.controller` and ORed together at query time.

Schema version 3 added this table and the `opencost_daily_costs (pod_id)`, `opencost_monthly_summaries (pod_id)` and `opencost_pods (cluster_id, controller)` indexes. Version 4 added `created_by` and `updated_by`, the Backstage user entity ref of the caller taken from the request credentials, so a preset can be traced to its author. Rows written before version 4 carry `null`.

## Design Decisions

**3NF normalization**: Pod metadata (`namespace`, `controller_kind`, `controller`) lives only in `opencost_pods`. Fact tables (`daily_costs`, `monthly_summaries`) reference via `pod_id` FK, eliminating daily/monthly duplication.

**Namespace in unique constraint**: `opencost_pods.UNIQUE(cluster_id, namespace, pod)` prevents same-name pods in different namespaces from colliding, a bug in the V1 schema where namespace was absent from the unique key.

**Timestamp semantics**: `created_at` records first insertion; `updated_at` records last upsert. V1's `collected_at` was overwritten on every upsert, losing the original collection timestamp.
