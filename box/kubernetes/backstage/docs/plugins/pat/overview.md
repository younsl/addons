---
plugins:
  - pat
  - pat-backend
---

# Personal Access Tokens

## Overview

Personal access tokens (PATs) let another system call this Backstage instance's plugin APIs without a browser session or a static `backend.auth.externalAccess` secret. An administrator issues a token in the UI, chooses which plugins it may reach and whether it may write, and sets a lifetime of at most one year. Every call made with a token, and every issue, revoke and delete, is written to an audit log that administrators can browse from the sidebar.

**Audience**

- **Administrators** issuing tokens to CI jobs, bots or partner services.
- **Integrators** calling the backend REST API with a token.
- **Plugin maintainers** deciding how their plugin treats token-authenticated calls.

## How a call is authenticated

A token is `bs_` followed by 43 random alphanumeric characters, just over 256 bits of entropy. The backend stores only its SHA-256 hash. A caller sends it as a bearer token to any plugin route:

```bash
curl -H "Authorization: Bearer bs_..." https://backstage.example.com/api/catalog/entities
```

The root HTTP router runs a gateway middleware before any plugin router. It ignores every bearer token that does not start with the prefix, so user sessions and plugin-to-plugin tokens are untouched. For a PAT it:

1. Looks the hash up and rejects unknown, revoked or expired tokens with 401.
2. Reads the target plugin id from the path (`/api/<plugin>/...`) and checks the token's scopes. A missing scope, a write with a read-only scope, or any call to the `pat` plugin itself is rejected with 403.
3. Mints a plugin-to-plugin token for the target plugin and replaces the `Authorization` header with it, adding `x-backstage-pat-id` and `x-backstage-pat-name` so the plugin can attribute the call.
4. Records an `api.request` audit event when the response finishes, with the real status code and latency, and updates the token's last-used time and call count.

The target plugin therefore sees the `plugin:pat` service principal. In-house plugins already accept a service principal on GET with admin visibility and reject it on other methods, so a write scope only takes effect on plugins that accept writes from services (catalog locations, scaffolder tasks). Read scopes work everywhere.

Repeated invalid tokens from one client address are throttled to 30 per minute with 429.

## Scopes

A scope is one plugin id plus an access level.

| Access | Methods allowed |
|---|---|
| `read` | GET, HEAD, OPTIONS |
| `write` | Every method |

Only plugins listed under `pat.scopablePlugins` can be granted. The `pat` plugin is never scopable, so a token cannot list, create or revoke tokens. Duplicate plugins in a request collapse to the highest access.

## Lifetime

`expiresInDays` must be an integer from 1 to `pat.maxExpiryDays`. The config value is itself capped at 365, so no token outlives a year regardless of configuration. Tokens cannot be renewed. Issue a new one and revoke the old.

## Administration

The **Access Tokens** entry under **Administration** in the sidebar is visible only to users listed in `permission.admins`. Its badge counts denied token calls in the last 24 hours. The page has two tabs. The backend enforces the same list on every route. Service principals and anonymous callers are rejected even when `backend.auth.dangerouslyDisableDefaultAuthPolicy` is on, which differs from the older in-house plugins that fall back to guest in that mode.

- **Tokens** (`/pat`) lists tokens with state, scopes, expiry, last use and call count. The create dialog requires a name (letters, digits, hyphen and underscore only), a description, a lifetime typed in days or picked from the calendar, and at least one scope before the button enables. The secret is shown once after creation.
- **Token detail** (`/pat/tokens/:id`), opened by clicking a row, edits the name, description and permissions of an active token in place and shows its last 20 audit events. Lifetime cannot be changed. Revoked and expired tokens are read-only. Revoke and Delete live here as well.
- **Audit Log** (`/pat/audit`) lists events newest first with filters by event type, outcome and free text, pagination, and an optional 15-second auto refresh. The configured retention period is shown above the table.

Revoking and deleting both ask the admin to retype the token name. Deleting an active token stops it authenticating immediately, the same as revoking, and removes it from the list. Audit rows keep the token id and name so history survives deletion, and the `token.deleted` event records the state the token was in.

## Schema

Table `pat_tokens`:

| Column | Notes |
|---|---|
| `id` | UUID primary key |
| `name`, `description` | Admin-provided, both required. Name is 1 to 100 characters from `A-Z a-z 0-9 - _` only |
| `token_hash` | SHA-256 hex of the full token, unique |
| `token_prefix` | First 11 characters, for display |
| `scopes` | JSON array of `{plugin, access}` |
| `created_by`, `created_at` | Admin entity ref and ISO time |
| `expires_at` | ISO time, at most 365 days after creation |
| `revoked_at`, `revoked_by` | Null while active |
| `last_used_at`, `last_used_ip`, `use_count` | Updated by the gateway |

Table `pat_audit_events`:

| Column | Notes |
|---|---|
| `event_type` | `token.created`, `token.updated`, `token.revoked`, `token.deleted`, `api.request`, `api.denied` |
| `outcome`, `reason` | `allowed` or `denied`, reason such as `expired` or `scope_insufficient` |
| `token_id`, `token_name`, `actor` | Actor is the admin ref for lifecycle events, `pat:<id>` for calls |
| `plugin_id`, `method`, `path`, `status_code`, `duration_ms` | Request details for call events |
| `ip`, `user_agent` | From `X-Forwarded-For` when present |
| `details` | JSON `{ before, after }` of the changed fields on `token.updated`, null otherwise |
| `created_at` | ISO time, indexed |

Events older than `pat.audit.retentionDays` (default 365) are purged daily at 03:30 UTC.

## API

All routes except `/health` require an admin user token.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/pat/admin-status` | `{ isAdmin }` for the current user |
| GET | `/api/pat/settings` | Max lifetime, audit retention days and scopable plugins |
| GET | `/api/pat/tokens` | List tokens, never the secret |
| POST | `/api/pat/tokens` | Create, returns `{ token, record }` once |
| GET | `/api/pat/tokens/:id` | One token |
| PATCH | `/api/pat/tokens/:id` | Edit `name`, `description`, `scopes` of an active token |
| POST | `/api/pat/tokens/:id/revoke` | Revoke |
| DELETE | `/api/pat/tokens/:id` | Delete a token in any state |
| GET | `/api/pat/audit` | `limit`, `offset`, `tokenId`, `eventType`, `outcome`, `search` |
| GET | `/api/pat/audit/summary` | 24-hour call and token counts |

## Configuration

```yaml
app:
  plugins:
    pat: true
permission:
  admins:
    - user:default/alice
pat:
  maxExpiryDays: 365
  audit:
    retentionDays: 365
  scopablePlugins:
    - id: catalog
      label: Catalog
      description: Entities, locations and refresh
    - search
```

Entries may be a plain plugin id or an object with `id`, `label` and `description`.
