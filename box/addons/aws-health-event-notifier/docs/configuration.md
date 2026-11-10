# Configuration

All settings are CLI flags backed by environment variables. The Helm chart sets them from [values.yaml](../charts/aws-health-event-notifier/values.yaml).

| Env | Default | Description |
|-----|---------|-------------|
| SLACK_WEBHOOK_URL | _(required)_ | Slack Incoming Webhook URL |
| POLL_INTERVAL_SECS | 60 | Poll interval |
| INITIAL_LOOKBACK_SECS | 3600 | Lookback window when the ledger has no cursor yet |
| COLD_START_SUPPRESS | true | Record the backlog without sending when the ledger has no cursor yet |
| ALLOW_CATEGORIES / DENY_CATEGORIES | _(empty)_ | Filter by event type category |
| ALLOW_SERVICES / DENY_SERVICES | _(empty)_ | Filter by AWS service code |
| ALLOW_REGIONS / DENY_REGIONS | _(empty)_ | Filter by AWS region code (`global` always allowed unless denied) |
| ALLOW_EVENT_CODES / DENY_EVENT_CODES | _(empty)_ | Filter by `SERVICE/EVENT_TYPE_CODE` pair (e.g. VPN/AWS_VPN_REDUNDANCY_LOSS) |
| REMINDER_OFFSETS_HOURS | 24 | Reminder offsets before start time |
| LEADER_ELECTION | false | Elect one active replica through a Lease, record deliveries in a ConfigMap |
| LEASE_NAME / LEDGER_CONFIGMAP | aws-health-event-notifier-leader / -ledger | Lease and ledger ConfigMap names |
| LEASE_DURATION_SECS / RENEW_DEADLINE_SECS / RETRY_PERIOD_SECS | 15 / 10 / 2 | Election timing |
| ADMIN_ADDR | 0.0.0.0:8081 | Admin server (/healthz, /readyz, /metrics) |
