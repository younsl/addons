# backstage-mcp

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-backstage--mcp-black?style=flat-square&logo=docker&logoColor=white)](https://github.com/younsl/addons/pkgs/container/backstage-mcp)
[![Helm Chart](https://img.shields.io/badge/ghcr.io-charts%2Fbackstage--mcp-black?style=flat-square&logo=helm&logoColor=white)](https://github.com/younsl/addons/pkgs/container/charts%2Fbackstage-mcp)
[![Rust](https://img.shields.io/badge/rust-1.98.1-black?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=black)](https://github.com/younsl/addons/blob/main/LICENSE)

Read-only [Model Context Protocol](https://modelcontextprotocol.io) server for the [Backstage](../README.md) instance in this directory. It turns the catalog, search, TechDocs and every in-house plugin page into tools an AI agent such as [kagent](https://kagent.dev) can call, and it never writes anything back. Built with Rust 1.98 on [rmcp](https://github.com/modelcontextprotocol/rust-sdk) and shipped as a statically linked musl binary on a scratch image, cross-compiled with cargo-zigbuild.

## How it works

The server runs as its own Deployment next to Backstage and calls the Backstage backend REST API over the cluster network, so no Backstage rebuild is needed to change a tool. Every tool is an HTTP GET followed by filtering, sorting and paging done in the server, because most plugin endpoints return every row at once and an agent needs a bounded slice of them.

1. A kagent Agent references the server through a `RemoteMCPServer` resource and calls a tool over Streamable HTTP, presenting the bearer token from `MCP_BEARER_TOKEN`.
2. The server issues one or more GETs to Backstage with the static external-access token from `BACKSTAGE_TOKEN`.
3. The response is reduced to what the question needs (a page of rows, a summary, a page of docs as text) and returned as the tool result. Backstage errors come back as tool errors with the status and message, so the model can adjust instead of failing the turn.

Serving is stateless: every MCP request is answered on its own, so replicas scale freely and any replica can answer any request. The readiness probe follows Backstage's own readiness endpoint, so a replica that cannot reach the portal leaves the Service.

## Tools

67 tools, all annotated `readOnlyHint: true`. Names are prefixed by the Backstage page they read from.

| Page | Tools |
| --- | --- |
| Catalog, APIs | `catalog_search_entities`, `catalog_get_entity`, `catalog_list_facets`, `catalog_get_api_definition` |
| Search | `search_query` (software-catalog and techdocs indexes) |
| Docs | `techdocs_get_metadata`, `techdocs_get_page` (HTML rendered to text with headings and fenced code) |
| Platforms | `platforms_get_stats` |
| API Registry | `openapi_registry_list_registrations`, `openapi_registry_get_registration`, `openapi_registry_get_entity_yaml` |
| Catalog Health | `catalog_health_get_coverage`, `catalog_health_list_projects`, `catalog_health_list_groups`, `catalog_health_get_history`, `catalog_health_list_branches` |
| ArgoCD | `argocd_get_status`, `argocd_list_application_sets`, `argocd_get_application_set`, `argocd_list_upstream_charts`, `argocd_get_upstream_chart`, `argocd_get_upstream_scan_status`, `argocd_list_audit_logs`, `argocd_list_repo_branches` |
| GitLab Tokens | `gitlab_token_audit_get_status`, `gitlab_token_audit_list_tokens`, `gitlab_token_audit_get_webhook`, `gitlab_token_audit_list_notifications` |
| Cost Report | `opencost_get_config`, `opencost_list_filters`, `opencost_list_years`, `opencost_search_controllers`, `opencost_get_monthly_totals`, `opencost_get_daily_summary`, `opencost_list_monthly_pod_costs`, `opencost_list_daily_pod_costs`, `opencost_get_pod_daily_costs`, `opencost_list_collection_runs` |
| IAM Audit | `iam_audit_get_status`, `iam_audit_list_users`, `iam_audit_list_password_reset_requests`, `iam_audit_get_password_reset_request`, `iam_audit_list_muted_users`, `iam_audit_get_warning_dm_logs`, `iam_audit_get_slack_health` |
| OpenSearch | `opensearch_account_get_config`, `opensearch_account_list_accounts`, `opensearch_account_list_roles`, `opensearch_account_list_requests`, `opensearch_account_get_request`, `opensearch_viewer_get_config`, `opensearch_viewer_list_snapshots`, `opensearch_viewer_get_snapshot` |
| Capacity | `opensearch_scaling_get_config`, `opensearch_scaling_list_domains`, `opensearch_scaling_get_domain`, `opensearch_scaling_list_requests` |
| S3 Log Extract | `s3_log_extract_get_config`, `s3_log_extract_list_apps`, `s3_log_extract_precheck`, `s3_log_extract_list_requests`, `s3_log_extract_get_request` |
| Access Tokens | `pat_get_settings`, `pat_list_tokens`, `pat_get_token`, `pat_list_audit_events`, `pat_get_audit_summary` |

List tools take `offset` and `limit` and report `total` and `truncated`, so an agent pages instead of raising the limit. A result longer than `MAX_RESULT_CHARS` is cut with a note asking the model to narrow the query. Secrets never leave the server: the GitLab token audit webhook URL is reduced to a configured flag, the S3 archive and its password are not reachable at all, and a personal access token is only ever reported by its short non-secret prefix, because Backstage stores a hash and shows the secret once at creation.

`backstage-mcp --list-tools` prints the registered names, and `tools/list` on the endpoint returns the JSON schemas the descriptions above are generated from.

## Backstage side

Two things on the Backstage instance make the tools work, both already in this repository from image `1.54.6-1`.

**A static external-access token.** Backstage authenticates services through `backend.auth.externalAccess`. Declare one and hand the same value to this server as `BACKSTAGE_TOKEN`. The `accessRestrictions` block limits which plugins the token can reach, which is how the read-only promise is enforced on the Backstage side as well: a token that can only reach these plugins cannot call the scaffolder or the permission API.

```yaml
backend:
  auth:
    externalAccess:
      - type: static
        options:
          token: ${BACKSTAGE_MCP_TOKEN}
          subject: backstage-mcp
        accessRestrictions:
          - plugin: catalog
          - plugin: search
          - plugin: techdocs
          - plugin: platforms
          - plugin: openapi-registry
          - plugin: catalog-health
          - plugin: argocd-appset
          - plugin: gitlab-token-audit
          - plugin: opencost
          - plugin: iam-user-audit
          - plugin: opensearch-account
          - plugin: opensearch-viewer
          - plugin: opensearch-scaling
          - plugin: s3-log-extract
          - plugin: pat
```

**Plugins that accept a service principal on reads.** The in-house plugins used to accept only signed-in users (`allow: ['user']`), so an external token got 401 from every list endpoint. Since `1.54.6-1` their auth helpers also accept a service principal on GET requests, with admin visibility so list endpoints return every row, and reject it on any other method, so the token can never approve, mute, reserve or download anything even if it were pointed at those routes.

The `pat` plugin shipped in `1.54.7-1` without that helper and refused every service principal. It gained it after that tag, so the `pat_*` tools need a Backstage image built from `plugins/pat-backend` at or after that change. Against `1.54.7-1` they answer 403. Dropping `- plugin: pat` from `accessRestrictions` keeps the plugin unreachable for deployments that would rather not put token metadata in an agent's context. A personal access token can never reach the tools' own plugin the other way round either: `pat` is refused as a scope target, so one token cannot enumerate or audit another.

## Configuration

Every setting is an environment variable.

| Variable | Default | Purpose |
| --- | --- | --- |
| `BACKSTAGE_URL` | required | Backstage backend base URL, for example `http://backstage.backstage.svc:7007` |
| `BACKSTAGE_TOKEN` | empty | Static external-access token sent as a bearer token. Empty sends unauthenticated requests, which only the endpoints without auth checks answer. |
| `REQUEST_TIMEOUT_SECONDS` | `30` | Bound on one request to Backstage |
| `MCP_TRANSPORT` | `http` | `http` for Streamable HTTP, `stdio` for a local client that spawns the binary (`--stdio` does the same) |
| `LISTEN_PORT` | `8080` | HTTP listener for the MCP endpoint and the probes |
| `MCP_PATH` | `/mcp` | Path the MCP endpoint is served on |
| `MCP_BEARER_TOKEN` | empty | Bearer token MCP clients must present. Empty disables inbound authentication. |
| `MAX_RESULT_CHARS` | `100000` | Upper bound on the characters of one tool result |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn` or `error` |
| `LOG_FORMAT` | `json` | `json` or `text` |

`GET /healthz` always answers 200 once the process is up. `GET /readyz` proxies Backstage's `/.backstage/health/v1/readiness` and answers 503 when it fails.

## Deploy

Install the [chart](charts/backstage-mcp/README.md) in the namespace where the kagent Agents live, because kagent resolves the `headersFrom` Secret of a `RemoteMCPServer` in the Agent's namespace.

```bash
helm install backstage-mcp oci://ghcr.io/younsl/charts/backstage-mcp \
  --namespace kagent \
  --set backstage.url=http://backstage.backstage.svc:7007 \
  --set backstage.token=$BACKSTAGE_MCP_TOKEN \
  --set mcp.bearerToken=$(openssl rand -hex 24) \
  --set kagent.remoteMCPServer.enabled=true
```

With `kagent.remoteMCPServer.enabled` the chart renders the `RemoteMCPServer` and an Agent references it by name:

```yaml
apiVersion: kagent.dev/v1alpha3
kind: Agent
metadata:
  name: platform-assistant
  namespace: kagent
spec:
  type: Declarative
  declarative:
    modelConfig: default-model-config
    systemMessage: |
      You answer questions about services, owners, costs and platform state using the Backstage tools. You cannot change anything.
    tools:
      - type: McpServer
        mcpServer:
          name: backstage-mcp
          kind: RemoteMCPServer
          toolNames:
            - catalog_search_entities
            - catalog_get_entity
            - search_query
            - techdocs_get_page
            - opencost_get_monthly_totals
```

Leave `toolNames` out to expose all 62 tools. A read-only agent in a production cluster is the intended shape, matching the pattern where prd kagent carries only read-only tool servers.

## Local development

```bash
make build            # debug binary
make test             # unit tests (wiremock stands in for Backstage)
make coverage         # cargo llvm-cov, fails under 70% line coverage
make lint             # clippy with pedantic and nursery as errors
BACKSTAGE_TOKEN=... make run   # serve http://localhost:8080/mcp against http://localhost:7007
make list-tools       # print the registered tool names
```

For a local MCP client, run the binary with `--stdio`:

```json
{
  "mcpServers": {
    "backstage": {
      "command": "/path/to/backstage-mcp",
      "args": ["--stdio"],
      "env": { "BACKSTAGE_URL": "http://localhost:7007", "BACKSTAGE_TOKEN": "..." }
    }
  }
}
```

## Release

Bump `org.opencontainers.image.version` in the `Dockerfile` to publish `ghcr.io/younsl/backstage-mcp`, and `version` in `charts/backstage-mcp/Chart.yaml` to publish the chart. Both release on merge to `main` through the shared Rust scratch container and chart workflows.
