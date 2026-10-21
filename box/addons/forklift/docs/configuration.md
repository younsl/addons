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
| `FORKLIFT_STORAGE_S3_PREFIX` / `_REGION` / `_ENDPOINT` | | Key prefix, AWS region, custom endpoint for S3-compatible stores |
| `FORKLIFT_STORAGE_S3_FORCE_PATH_STYLE` | `false` | Path-style addressing (most self-hosted stores need it) |
| `FORKLIFT_STORAGE_S3_ACCESS_KEY_ID` / `_SECRET_ACCESS_KEY` | (default chain) | Static keys; empty uses IRSA / EKS Pod Identity |
| `FORKLIFT_STORAGE_S3_PROVIDER` | `aws` without an endpoint, `minio` with one | `aws`, `minio`, `rustfs`, `seaweedfs`, `garage` or `generic`. Selects the admin API the Storage page reads |
| `FORKLIFT_STORAGE_S3_ADMIN_ENDPOINT` | | Admin API base URL when it is not the S3 endpoint: SeaweedFS master (`http://master:9333`) or Garage admin (`http://garage:3903`) |
| `FORKLIFT_STORAGE_S3_ADMIN_TOKEN` | | Garage admin bearer token |
| `FORKLIFT_STORAGE_S3_CREATE_BUCKET` | `false` | Create the bucket at boot when it is missing. The chart turns it on for the bundled SeaweedFS |
| `FORKLIFT_STORAGE_S3_READ_TIMEOUT` | `10m` | Longest wait for an S3 response to start, upload body included, so a request to a store that went away fails and retries instead of hanging. Connecting times out after 10s. `migrate-storage` takes `FORKLIFT_MIGRATE_TO_S3_READ_TIMEOUT` for the target |

`forklift migrate-storage` additionally reads the target from `FORKLIFT_MIGRATE_TO_S3_BUCKET`, `_PREFIX`, `_REGION`, `_ENDPOINT`, `_FORCE_PATH_STYLE`, `_ACCESS_KEY_ID`, `_SECRET_ACCESS_KEY`, `_CREATE_BUCKET`, `_PROVIDER`, `_ADMIN_ENDPOINT` and `_ADMIN_TOKEN`. See [Storage migration](storage-migration.md).
| `FORKLIFT_STORAGE_META_SYNC_INTERVAL` | `30s` | s3-mode standby poll cadence; must stay shorter than the fixed 24h blob GC grace period. In `snapshot` mode it is also the upload cadence (failover data-loss window), and uploads are paced to about 4 GiB an hour, so after a large upload the next one can wait up to 15m. In `wal` mode the leader ships every 5s |
| `FORKLIFT_STORAGE_META_SYNC_MODE` | `wal` | s3-mode metadata publishing: `wal` ships committed WAL frames as append-only segments on a base copy (see `docs/designs/wal-shipping.md`), `snapshot` uploads a whole snapshot per change. Readers follow either, so it can change between terms |
| `FORKLIFT_STORAGE_META_COMPRESSION` | `zstd` | s3-mode snapshot and base upload encoding, `zstd` or `none`. Readers detect the encoding. Before rolling back below 0.18.0, set `none` and `FORKLIFT_STORAGE_META_SYNC_MODE=snapshot` and let one upload complete, or the older version cannot restore the metadata |
| `FORKLIFT_HTTP_ADDR` | `:8080` | API, UI and package endpoints |
| `FORKLIFT_METRICS_ADDR` | `:8081` | Prometheus metrics |
| `FORKLIFT_PPROF_ADDR` | `127.0.0.1:6060` | `pprof`-compatible CPU profile listener (`/debug/pprof/profile`), loopback-only so it is reachable through `kubectl port-forward` but not through the Service; empty disables it |
| `FORKLIFT_EXTERNAL_URL` | (request-derived) | Base URL for URLs synthesised in package metadata; set behind a reverse proxy instead of relying on `X-Forwarded-*` |
| `FORKLIFT_EXTRA_CA_FILE` | (empty) | PEM bundle of private CA certificates trusted by every outbound client (upstreams, OSV, deps.dev, OIDC, webhooks, GitLab, S3) on top of the system roots. A missing, malformed or empty file fails startup |
| `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` / `NO_PROXY` | (empty) | Standard proxy variables, honored by every outbound client including S3. The startup log line `outbound http configured` reports whether a proxy is set and how many extra CA certificates were loaded, never the proxy URL |
| `FORKLIFT_LOG_LEVEL` / `FORKLIFT_LOG_FORMAT` | `info` / `json` | Logging. Every line carries `time`, `level`, `component` (the module it comes from, such as `migrate`, `objstore` or `api`, `main` for startup) and `msg` |
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
| `FORKLIFT_AUDIT_DOWNLOAD_RETENTION` | `168h` (7d) | Shorter retention for artifact download entries, the bulk of the log. Refused downloads (401, 403) keep `FORKLIFT_AUDIT_RETENTION` for the security insights, and artifact download counts come from daily totals kept 30 days, so neither is affected. `0`, or a value not below `FORKLIFT_AUDIT_RETENTION`, applies `FORKLIFT_AUDIT_RETENTION` |
| `FORKLIFT_INSIGHTS_ENABLED` | `true` | Run the periodic security insight analysis on the leader ([docs](security-insights.md)) |
| `FORKLIFT_INSIGHTS_INTERVAL` | `1h` | How often the leader analyzes until the interval is saved on the Insights Settings page, which then takes over. `0` starts with the scheduled analysis off |
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
