# MCP Server

trivy-collector can expose its report store to LLM agents through an embedded [MCP](https://modelcontextprotocol.io/) server. The endpoint runs inside the `server` pod, reads the same SQLite database as the REST API, and is protected by the same authentication and RBAC. No extra Deployment or image is needed.

## Overview

![trivy-collector MCP architecture](assets/4-mcp-architecture.svg)

| Item | Value |
|------|-------|
| Path | `/mcp` on the server port (default `3000`) |
| Transport | MCP Streamable HTTP (`POST`, `GET` for SSE, `DELETE` for session teardown) |
| Protocol revisions | `2025-03-26`, `2025-06-18` (session mode), `2026-07-28` (stateless) |
| Tools | 9 read-only tools, see [Tools](#tools) |
| Default | Disabled (`MCP_ENABLED=false`, `server.mcp.enabled: false`) |

The server prefers `Content-Type: application/json` for plain request/response tool calls and falls back to `text/event-stream` only when it has to stream notifications. Clients must send `Accept: application/json, text/event-stream`.

## Enabling

Helm:

```bash
helm upgrade --install trivy-collector oci://ghcr.io/younsl/charts/trivy-collector \
  --namespace trivy-system \
  --reuse-values \
  --set server.mcp.enabled=true
```

Environment variables (when not using Helm):

| Variable | Default | Description |
|----------|---------|-------------|
| `MCP_ENABLED` | `false` | Mount the `/mcp` endpoint |
| `MCP_ALLOWED_HOSTS` | `""` | Comma-separated `Host` header allow-list. Empty disables validation |
| `MCP_STATELESS` | `false` | Serve without `Mcp-Session-Id` sessions |
| `MCP_MAX_CONCURRENCY` | `8` | Tool calls executing at once across all sessions. `0` disables the cap. Not exposed as a chart value, set it through the server container environment if you need to change it |

### Host validation

The MCP transport ships with DNS-rebinding protection that only accepts loopback `Host` headers. trivy-collector disables that check by default because in-cluster clients reach the pod through a Service DNS name such as `trivy-collector.trivy-system.svc.cluster.local:3000`. If `/mcp` is exposed through a Gateway and you want the check back, set `server.mcp.allowedHosts` to the public hostnames.

### Sessions and replicas

In the default session mode the server issues an `Mcp-Session-Id` on `initialize` and keeps per-session state in memory. That state is not shared between pods, so with `server.replicaCount` above 1 set `server.mcp.stateless: true`. Stateless mode answers every request independently and never issues a session id.

## Authentication and Authorization

The MCP endpoint sits inside the same middleware stack as `/api/v1/*`.

**Authentication.** With `auth.mode=keycloak` every MCP request must carry `Authorization: Bearer <token>`. Use a self-issued API token (`tc_...`) created from the Auth page or `POST /api/v1/auth/tokens`, see [Authentication](authentication.md). Unauthenticated requests get `401` with a JSON body, never a login redirect. With `auth.mode=none` no header is required.

**Authorization.** Two checks apply:

1. Endpoint gate. Any request to `/mcp` requires `reports:get`. Without it the transport returns `403` before the MCP handshake starts.
2. Per-tool check. Each tool re-evaluates the caller's groups against the same policy CSV using the resource and action of the REST endpoint it mirrors. A denied call returns a JSON-RPC error with `RBAC denied: <resource>:<action>` and HTTP `200`, as the protocol requires.

The built-in `role:readonly` already grants `reports:get`, `clusters:get`, and `stats:get`, so no policy change is needed for read access. API tokens carry a snapshot of the issuer's Keycloak groups taken at creation time, so a token minted by an admin keeps admin roles and a token minted by a read-only user stays read-only. Tokens created before this snapshot existed have no groups and fall back to the default policy (`RBAC_DEFAULT_POLICY`). Re-create them to pick up group roles.

## Concurrency, Metrics, and Audit Log

Tool calls share one concurrency limit across every session (`MCP_MAX_CONCURRENCY`, default 8). Calls beyond the limit queue rather than fail, so an agent that fans out many parallel tool calls cannot starve the SQLite pool the UI uses. Queue time counts toward the duration metric.

Every tool call records:

| Metric | Labels | Description |
|--------|--------|-------------|
| `trivy_collector_mcp_tool_calls_total` | `tool`, `result` | `result` is one of `success`, `denied`, `invalid_params`, `not_found`, `error` |
| `trivy_collector_mcp_tool_duration_seconds` | `tool` | Histogram, includes time spent waiting for a concurrency slot |
| `trivy_collector_mcp_tool_calls_in_flight` | none | Gauge of tool bodies currently executing |

Each call also writes one row to the API audit log with method `MCP` and path `/mcp/tools/call/<tool>`, carrying the caller's subject, email, `User-Agent`, `X-Forwarded-For`, duration, and an HTTP-style status (`200`, `400`, `403`, `404`, `500`). Filter the Admin console by path prefix `/mcp/` to see agent activity next to REST traffic. Transport-level requests (`initialize`, `tools/list`, SSE streams) are not logged.

## Tools

All tools are read-only and advertise `readOnlyHint: true`. List-style tools accept `limit` and `offset`, clamp `limit` on the server, and return `total` and `truncated` so an agent knows when to page.

| Tool | RBAC | Arguments | Returns |
|------|------|-----------|---------|
| `list_clusters` | `clusters:get` | none | Registered clusters with report counts and `last_seen` |
| `list_namespaces` | `clusters:get` | `cluster?` | Namespaces that have reports |
| `get_stats` | `stats:get` | none | Fleet totals by report type and severity |
| `list_vulnerability_reports` | `reports:get` | `cluster?`, `namespace?`, `app?`, `image?`, `severity?[]`, `limit` (max 100), `offset` | Report metadata with severity summary |
| `list_sbom_reports` | `reports:get` | `cluster?`, `namespace?`, `app?`, `image?`, `component?`, `limit` (max 100), `offset` | Report metadata with component count |
| `get_vulnerability_report` | `reports:get` | `cluster`, `namespace`, `name`, `severity?[]`, `fixed_only?`, `limit` (max 200), `offset` | Report metadata plus paged findings (`id`, `severity`, `score`, `package`, `installed_version`, `fixed_version`, `title`, `link`) |
| `get_sbom_report` | `reports:get` | `cluster`, `namespace`, `name`, `component?`, `limit` (max 200), `offset` | Report metadata plus paged components (`name`, `version`, `type`, `purl`) |
| `search_vulnerabilities` | `reports:get` | `query` (CVE id or package substring), `limit` (max 100), `offset` | One row per affected image |
| `search_sbom_components` | `reports:get` | `component` (substring), `version?` (exact), `limit` (max 100), `offset` | One row per image containing the component |
| `list_alert_rules` | `alerts:get` | `package?` (substring), `enabled_only?`, `not_ready_only?`, `limit` (max 100), `offset` | One row per rule: matcher, receiver count, `ready`, `last_fired_at`, `fired_count` |
| `get_alert_rule` | `alerts:get` | `name` | One rule in full, including its status subresource |

Slack webhook URLs never appear in tool output. `get_alert_rule` replaces each one with `[redacted]` rather than dropping the field, so a caller can still see that a Slack destination is configured. A webhook URL is a credential for posting into a channel, and tool output goes straight into a model's context.

Both alert tools need the Kubernetes API, so they fail with an "unavailable" message when the pod has no API access or the `AlertRule` CRD is not installed. They never answer with an empty list, which would read as "the fleet has no rules". See [Alerts](alerts.md).

`not_ready_only` is the one worth asking an agent for: it returns rules the evaluator refuses to act on, which look enabled but never fire.

Full report JSON is never returned in one call. `get_*_report` tools return metadata plus a paged slice, which keeps a single tool result small enough for an agent context window even for SBOMs with thousands of components.

## kagent Setup

[kagent](https://kagent.dev/) connects to external MCP servers through the `RemoteMCPServer` resource. Static headers come from a Secret, which is where the trivy-collector API token goes.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: trivy-collector-mcp
  namespace: kagent
stringData:
  authorization: "Bearer tc_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
---
apiVersion: kagent.dev/v1alpha2
kind: RemoteMCPServer
metadata:
  name: trivy-collector
  namespace: kagent
spec:
  description: Multi-cluster Trivy vulnerability and SBOM reports
  protocol: STREAMABLE_HTTP
  url: http://trivy-collector.trivy-system.svc.cluster.local:3000/mcp
  headersFrom:
    - name: Authorization
      valueFrom:
        type: Secret
        valueRef: trivy-collector-mcp
        key: authorization
  timeout: 30s
  sseReadTimeout: 5m
```

Omit `headersFrom` when `auth.mode=none`. Adjust the Service name and namespace to your release. Then reference the server from an `Agent` under `spec.tools` as a `McpServer` tool source and pick the tools you want the agent to see.

Token checklist:

- Create the token as a user whose effective role covers `reports:get`, `clusters:get`, `stats:get`. With the default policy any authenticated user qualifies.
- Pick the longest expiry you are comfortable rotating (up to 365 days). An expired token makes every MCP call fail with `401`.
- Store the `Bearer ` prefix inside the Secret value so kagent can inject the header verbatim.

## Manual Testing

Any Streamable HTTP client works. With `curl`:

```bash
URL=http://localhost:3000/mcp
AUTH="Authorization: Bearer tc_..."   # drop when auth.mode=none

# 1. initialize, capture the session id
SESSION=$(curl -s -D - -o /dev/null "$URL" -H "$AUTH" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  | awk 'tolower($1)=="mcp-session-id:"{print $2}' | tr -d '\r')

# 2. list tools
curl -s "$URL" -H "$AUTH" -H "Mcp-Session-Id: $SESSION" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'

# 3. call a tool
curl -s "$URL" -H "$AUTH" -H "Mcp-Session-Id: $SESSION" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_vulnerabilities","arguments":{"query":"CVE-2024-3094","limit":5}}}'
```

Responses arrive as `text/event-stream` in session mode. Read the last `data:` line for the JSON-RPC result.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `404` on `/mcp` | MCP disabled | Set `server.mcp.enabled: true` or `MCP_ENABLED=true` |
| `401` JSON body | Missing or expired Bearer token under `auth.mode=keycloak` | Issue a new API token, check the `Authorization` header reaches the pod |
| `403` with `"resource":"reports"` | Caller lacks `reports:get`, which gates the `/mcp` endpoint itself, not just the report tools | Grant `role:readonly` or set `RBAC_DEFAULT_POLICY` |
| `403` with `"resource":"alerts"` | Caller reached `/mcp` but lacks `alerts:get` | Grant `alerts:get`; `role:readonly` already has it |
| `403` or `421` mentioning Host | `MCP_ALLOWED_HOSTS` set but does not include the hostname clients use | Add the hostname or clear the variable |
| Tool result `RBAC denied: stats:get` | Custom policy grants `reports:get` but not the tool's resource | Extend the policy CSV |
| Session errors after a pod restart | Sessions are in memory | Client reconnects and re-initializes, or enable `server.mcp.stateless` |
| Intermittent failures with several replicas | Session pinned to one pod | Set `server.mcp.stateless: true` |
