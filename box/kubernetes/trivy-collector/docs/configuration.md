# Configuration

## Command-line Options

| Argument | Environment Variable | Default | Mode | Description |
|----------|---------------------|---------|------|-------------|
| `--mode` | `MODE` | `collector` | Both | Deployment mode: `collector` or `server` |
| `--log-format` | `LOG_FORMAT` | `json` | Both | Log format: `json` or `pretty` |
| `--log-level` | `LOG_LEVEL` | `info` | Both | Log level: trace, debug, info, warn, error |
| `--health-port` | `HEALTH_PORT` | `8080` | Both | Health check server port |

## Watch Options (scraper mode)

| Argument | Environment Variable | Default | Description |
|----------|---------------------|---------|-------------|
| `--cluster-name` | `CLUSTER_NAME` | `local` | Name recorded on reports from the hub's own cluster |
| `--namespaces` | `NAMESPACES` | `""` | Namespaces the local watcher scans (comma-separated, empty = all). Edge clusters carry their own list in their registration Secret |
| `--collect-vulnerability-reports` | `COLLECT_VULN` | `true` | Watch VulnerabilityReports. Applies to the hub and every registered edge |
| `--collect-sbom-reports` | `COLLECT_SBOM` | `true` | Watch SbomReports. Applies to the hub and every registered edge |

Disabling a report kind stops the watch for it on every cluster and marks it as nothing-to-wait-for in hydration accounting, so `/readyz` still clears.

## Server Mode Options

| Argument | Environment Variable | Default | Description |
|----------|---------------------|---------|-------------|
| `--server-port` | `SERVER_PORT` | `3000` | API/UI server port |
| `--scraper-url` | `SCRAPER_URL` | `http://localhost:8081` | Base URL of the scraper's internal API. The server holds no database, so this is required |
| `--notes-configmap` | `NOTES_CONFIGMAP` | `trivy-collector-notes` | ConfigMap holding report notes |
| `--api-tokens-secret` | `API_TOKENS_SECRET` | `trivy-collector-api-tokens` | Secret holding API tokens |
| `--watch-local` | `WATCH_LOCAL` | `true` | Watch local cluster's Trivy reports |
| `--local-cluster-name` | `LOCAL_CLUSTER_NAME` | `local` | Local cluster name for K8s watching |
| `--mcp-enabled` | `MCP_ENABLED` | `false` | Mount the embedded MCP server at `/mcp` (see [MCP](mcp.md)) |
| `--mcp-allowed-hosts` | `MCP_ALLOWED_HOSTS` | `""` | Allowed `Host` values for `/mcp`, comma-separated. Empty disables the check |
| `--mcp-stateless` | `MCP_STATELESS` | `false` | Serve `/mcp` without sessions. Required with more than one server replica |
| `--mcp-max-concurrency` | `MCP_MAX_CONCURRENCY` | `8` | Concurrent MCP tool executions across all sessions. `0` = unlimited |

## Scraper Mode Options

| Argument | Environment Variable | Default | Description |
|----------|---------------------|---------|-------------|
| `--storage-path` | `STORAGE_PATH` | `/data` | Directory holding the SQLite database, on the scraper's `emptyDir`. Contents are rebuilt from the watched clusters on every start |
| `--internal-port` | `INTERNAL_PORT` | `8081` | Port the internal read API listens on |

## Shared Options

| Argument | Environment Variable | Default | Description |
|----------|---------------------|---------|-------------|
| `--internal-token` | `INTERNAL_TOKEN` | `""` | Shared token guarding the internal API, mounted into both pods from the same Secret. Empty makes the scraper reject every request |

## Subcommands

| Command | Description |
|---------|-------------|
| `version` | Print version, commit, and build date |
| `export-state --db-path <path> [--namespace <ns>] [--dry-run]` | One-shot migration off a PersistentVolume: read API tokens and report notes out of a legacy database and write them to the Secret and the ConfigMap. Reports need no export, since the next scraper start relists them |

## API Documentation

Server mode exposes auto-generated OpenAPI 3.1 spec via [utoipa](https://github.com/juhaku/utoipa) at `/api-docs/openapi.json`.

```bash
curl -s http://localhost:3000/api-docs/openapi.json | jq .
```

View with [Swagger Editor](https://editor.swagger.io) or import into Postman.

## Health Check Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/healthz` | GET | Liveness probe |
| `/readyz` | GET | Readiness probe |
