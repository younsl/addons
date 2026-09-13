# Configuration

## Overview

This document lists every process-level setting forklift reads at startup, with
its default and what it controls.

Read this when tuning an install, when a chart value does not expose the setting
you need, or when running the binary outside
[Kubernetes](https://github.com/kubernetes/kubernetes).

## Background

forklift is configured at two levels. Process-level settings are environment
variables read once at startup and cover addressing, storage, authentication,
logging, HA, and upload limits; the [Helm](https://github.com/helm/helm) chart
simply maps its values onto them. Repository-level settings such as caching, the
age policy, and the security policies are stored in the database and changed at
runtime through the UI or the REST API, so they are not listed here.

A few subsystems, notably vulnerability and license scanning, also accept
command-line flags. Where both exist the environment variable seeds the default
and the flag wins.

All settings are environment variables (the Helm chart maps values to them).

| Variable | Default | Description |
|----------|---------|-------------|
| `FORKLIFT_DATA_DIR` | `/data` | Root of the SQLite DB and (fs backend) blob store; `emptyDir` in s3 mode |
| `FORKLIFT_STORAGE_BACKEND` | `fs` | Blob + metadata backend: `fs` (PV) or `s3` (shared bucket, no EBS/RWX) |
| `FORKLIFT_STORAGE_S3_BUCKET` | (none) | S3 bucket (required when backend is `s3`) |
| `FORKLIFT_STORAGE_S3_PREFIX` / `_REGION` / `_ENDPOINT` | | Key prefix, AWS region, custom endpoint (MinIO) |
| `FORKLIFT_STORAGE_S3_FORCE_PATH_STYLE` | `false` | Path-style addressing (required by MinIO) |
| `FORKLIFT_STORAGE_S3_ACCESS_KEY_ID` / `_SECRET_ACCESS_KEY` | (default chain) | Static keys; empty uses IRSA / EKS Pod Identity |
| `FORKLIFT_STORAGE_META_SYNC_INTERVAL` | `30s` | s3-mode metadata snapshot cadence (failover data-loss window); must stay shorter than the fixed 24h blob GC grace period |
| `FORKLIFT_HTTP_ADDR` | `:8080` | API, UI and package endpoints |
| `FORKLIFT_METRICS_ADDR` | `:8081` | Prometheus metrics |
| `FORKLIFT_PPROF_ADDR` | `127.0.0.1:6060` | `pprof`-compatible CPU profile listener (`/debug/pprof/profile`), loopback-only so it is reachable through `kubectl port-forward` but not through the Service; empty disables it |
| `FORKLIFT_EXTERNAL_URL` | (request-derived) | Base URL for URLs synthesised in package metadata; set behind a reverse proxy instead of relying on `X-Forwarded-*` |
| `FORKLIFT_LOG_LEVEL` / `FORKLIFT_LOG_FORMAT` | `info` / `json` | Logging |
| `FORKLIFT_HA_ENABLED` | `false` | Enable Lease leader election |
| `FORKLIFT_REPLICATION_ENABLED` | `false` | Enable PV-based replication (requires HA) |
| `FORKLIFT_REPLICATION_TOKEN` | (none) | Shared token for internal replication endpoints |
| `FORKLIFT_REPLICATION_PEER_SERVICE` | (none) | Headless Service domain for peer pod DNS |
| `FORKLIFT_REPLICATION_INTERVAL` | `30s` | Standby pull cadence (data-loss window on failover) |
| `FORKLIFT_SESSION_SECRET` | (generated) | Signs session cookies; share across replicas |
| `FORKLIFT_ANONYMOUS_READ` | `false` | Allow unauthenticated pulls instance-wide. Individual repositories can be opened instead with the per-repository Public access toggle (Settings tab) |
| `FORKLIFT_SEED_DEFAULT_REPOS` | `true` | Seed default proxy + hosted repos on first run |
| `FORKLIFT_OCI_MAX_MANIFEST_BYTES` | `4194304` | Cap on one OCI manifest or index document |
| `FORKLIFT_OCI_MAX_BLOB_BYTES` | `0` | Cap on one pushed OCI blob (0 = unlimited) |
| `FORKLIFT_OCI_UPLOAD_SESSION_TTL` | `24h` | Age at which an incomplete OCI push session is pruned |
| `FORKLIFT_OCI_PRUNE_INTERVAL` | `1h` | How often the leader prunes unreachable OCI objects |
| `FORKLIFT_BOOTSTRAP_ADMIN_USER` / `_PASSWORD` | `admin` / (none) | Seed first admin on empty install |
| `FORKLIFT_OIDC_ENABLED` | `false` | Enable Keycloak OIDC login |
| `FORKLIFT_OIDC_ISSUER_URL` / `_CLIENT_ID` / `_CLIENT_SECRET` / `_REDIRECT_URL` | | OIDC settings |
| `FORKLIFT_OIDC_GROUPS_CLAIM` | `groups` | Claim mapped to roles |
| `FORKLIFT_RBAC_POLICY_FILE` | (none) | Path to a declarative policy.csv; enables declarative RBAC |
| `FORKLIFT_RBAC_DEFAULT_ROLE` | (none) | Role granted to every authenticated user (ArgoCD `policy.default`) |
| `FORKLIFT_RBAC_ACCOUNTS_DIR` | (none) | Directory of local-account password files (Secret mount) |
| `FORKLIFT_AUDIT_ENABLED` | `true` | Record per-repository audit events |
| `FORKLIFT_AUDIT_RETENTION` | `2160h` (90d) | Prune audit entries older than this; `0` keeps forever |
| `FORKLIFT_UI_UPLOAD_ENABLED` | `true` | Enable component-aware management API and web UI uploads; set `false` to opt out |
| `FORKLIFT_UI_UPLOAD_MAX_DURATION` | `30m` | Maximum duration of one upload request |
| `FORKLIFT_UI_UPLOAD_MAX_CONCURRENT` / `_USER` | `4` / `2` | Process-wide and per-principal upload concurrency limits |
| `FORKLIFT_UI_UPLOAD_MAX_ASSETS` | `16` | Maximum files in one managed publication |
| `FORKLIFT_UI_UPLOAD_MAX_FILE_BYTES` / `_BATCH_BYTES` | `256MiB` / `512MiB` | Per-file and aggregate upload byte limits |
| `FORKLIFT_UI_UPLOAD_GO_MAX_ZIP_BYTES` | `500MiB` | Go module ZIP limit, capped by the GOPROXY protocol |
| `FORKLIFT_COVERAGE_ENABLED` | `false` | Turn coverage scanning on. Off by default, so a GitLab token already in the environment never starts a crawl on its own |
| `FORKLIFT_COVERAGE_GITLAB_URL` | (none) | GitLab base URL the coverage scanner walks; empty disables coverage entirely |
| `FORKLIFT_COVERAGE_GITLAB_TOKEN` | (none) | GitLab access token with `read_api`; empty disables coverage entirely |

Per-repository options (caching and age policy) are set through the UI or the REST API, not env vars.

Vulnerability and license scanning are configured with process flags that take precedence over their matching env vars; see [Security policies](security-policies.md).

Forklift coverage takes only its GitLab connection from the environment. The
scope, schedule, exclusions and report receivers are stored in the database and
edited by an administrator in the console, so they are not listed here; see
[Coverage](coverage.md).

## Related documents

- [Installation](installation.md) for the chart values that map to these variables.
- [Access control](access-control.md) for the `FORKLIFT_RBAC_*` and OIDC variables in context.
- [Coverage](coverage.md) for the `FORKLIFT_COVERAGE_*` variables and everything they turn on.
