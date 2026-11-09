# Configuration

This reference lists every environment variable the gateway reads, with its default. It is for operators running the binary directly. The Helm chart sets the same variables from its values.

All settings come from environment variables.

## Slack

| Variable | Default | Description |
|----------|---------|-------------|
| `SLACK_BOT_TOKEN` | required | Bot token (`xoxb-...`). |
| `SLACK_DEFAULT_CHANNEL` | required | Channel used when the routing label is absent. A bare name is prefixed with `#`. A conversation ID is used as-is. |
| `SLACK_PARENT_MODE` | `lookup` | How the thread parent is obtained. `lookup` finds the message Alertmanager posted. `post` publishes the alert from the gateway. |
| `SLACK_LOOKUP_WINDOW` | `15m` | How far back channel history is searched. `lookup` mode only. |
| `SLACK_LOOKUP_ATTEMPTS` | `3` | How many times to re-search before giving up. `lookup` mode only. |
| `SLACK_INVESTIGATING_REACTION` | `telescope` | Emoji (without colons) placed on the alert notification while the agent investigates. Empty disables it. |
| `SLACK_COMPLETED_REACTION` | `white_check_mark` | Emoji (without colons) that replaces the investigating reaction once the analysis is posted. Empty disables it. When the reactions are enabled, a fixed thread note announcing the investigation is posted as well. |
| `SLACK_CHANNEL_LABEL` | `slack_channel` | Alert label that selects the destination channel. |
| `SLACK_CHANNEL_MAP` | empty | `label-value=channel` pairs, comma separated, for labels whose value is not the channel name. |
| `SLACK_MAX_TEXT` | `8000` | Maximum characters per message. Longer text is truncated. |
| `SLACK_API_URL` | `https://slack.com/api` | Web API base URL. Override to route through an egress proxy. Socket Mode dials the URL Slack hands out, honouring the standard proxy environment variables. |

## Mention invocation

All optional except the app-level token, which is what enables the feature.

| Variable | Default | Description |
|----------|---------|-------------|
| `SLACK_APP_TOKEN` | empty | App-level token (`xapp-...`). Empty leaves Socket Mode off and the whole mention path inert. |
| `CHAT_AGENT` | value of `KAGENT_AGENT` | Agent that answers mentions. |
| `CHAT_AGENT_MAP` | empty | `channel=agent` pairs, comma separated, routing a channel to a specialised agent. A channel the table does not carry falls back to `CHAT_AGENT`. |
| `CHAT_CHANNELS` | empty | Channel names or IDs allowed to invoke the bot. Empty allows every channel the bot is a member of. |
| `CHAT_ALLOWED_USERS` | empty | Slack member IDs allowed to invoke the bot. Empty allows everyone in the allowed channels. With `CHAT_ACCESS_POLICIES` it applies only to agents without an `AgentAccessPolicy`. |
| `CHAT_ACCESS_POLICIES` | `false` | Read per-agent allow lists from `AgentAccessPolicy` objects in `KAGENT_NAMESPACE`. See [Agent access policy](access-policy.md). |
| `CHAT_INSTRUCTIONS` | built-in English text | Instructions appended to every mention prompt. Separate from `ANALYSIS_INSTRUCTIONS`, because a question has no alert sections to fill. |
| `CHAT_USER_ID` | value of `KAGENT_USER_ID` | Sent as `X-User-Id` for mention turns. Splitting it from `KAGENT_USER_ID` keeps a person's questions and the unattended alert path on separate kagent sessions, which matters once agents carry long-term memory: memory is keyed by agent and user. |
| `CHAT_TIMEOUT` | `180s` | Deadline for one whole turn including queueing. Matches the controller's 3 minute cap. Lower it to make the gateway's own expiry fire first and cancel the task. |
| `CHAT_SESSION_TTL` | `2h` | How long a thread keeps its `contextId` after its last turn. `0s` makes every mention a cold turn. |
| `CHAT_STATUS_INTERVAL` | `10s` | How often the in-thread status message is rewritten while the agent works. Each rewrite is one `chat.update` call. |
| `CHAT_THREAD_HINT` | built-in text | Ephemeral hint sent when the bot is mentioned at channel level. Empty drops the mention silently. |
| `CHAT_DENIED_HINT` | built-in text | Ephemeral hint sent when the bot is mentioned in a channel outside `CHAT_CHANNELS`. Empty drops the mention silently. |
| `MAX_CONCURRENT_CHATS` | `2` | Maximum mention turns running at once. |

## kagent

| Variable | Default | Description |
|----------|---------|-------------|
| `KAGENT_URL` | `http://kagent-controller.kagent:8083` | Controller base URL. |
| `KAGENT_NAMESPACE` | `kagent` | Namespace of the Agent resource. |
| `KAGENT_AGENT` | `alert-triage-agent` | Agent used when the routing label is absent or names no mapped agent. |
| `KAGENT_AGENT_ROUTING_LABEL` | value of `SLACK_CHANNEL_LABEL` | Alert label that routes an alert to an agent. |
| `KAGENT_AGENT_ROUTING_MAP` | empty | `label-value=agent` routing table sending categories to specialised agents, e.g. `security-alerts=security-alert-triage-agent`. A value the table does not carry falls back to `KAGENT_AGENT` rather than being used as an agent name. |
| `KAGENT_USER_ID` | `gateway@kagent.dev` | Sent as `X-User-Id`. Owns the kagent sessions the gateway creates. |
| `KAGENT_TIMEOUT` | `120s` | Deadline for one whole analysis. On expiry the task is cancelled on the controller. |
| `KAGENT_REQUEST_TIMEOUT` | `30s` | Timeout for a single HTTP call to the controller (submit, poll, or cancel). |
| `KAGENT_POLL_INTERVAL` | `5s` | Wait between two task status polls. |

## Analysis policy

| Variable | Default | Description |
|----------|---------|-------------|
| `ANALYZE_SEVERITIES` | `critical` | Comma-separated severities to analyse. Empty analyses everything. |
| `ANALYZE_LABEL` | `analyze` | Alert label that opts a rule in (`"true"`) or out (`"false"`) regardless of severity. |
| `ANALYZE_RESOLVED` | `false` | Analyse resolved notifications as well as firing ones. |
| `DEDUPE_TTL` | `12h` | How long a group stays suppressed after being analysed. `0s` disables deduplication. |
| `MAX_ALERTS_IN_PROMPT` | `5` | Maximum alerts rendered into one prompt. |
| `MAX_CONCURRENT_ANALYSES` | `2` | Maximum analyses running at once, which bounds concurrent model spend. |
| `ANALYSIS_INSTRUCTIONS` | built-in English text | Instructions appended to every prompt. Override to change the output language or sections. |

## Serving

| Variable | Default | Description |
|----------|---------|-------------|
| `WEBHOOK_PATH` | `/alert` | Path Alertmanager posts to. |
| `WEBHOOK_BEARER_TOKEN` | empty | Bearer token required on incoming webhooks. Empty disables authentication. |
| `LISTEN_PORT` | `8080` | Port serving the webhook, `/healthz` and `/readyz`. |
| `METRICS_PORT` | `8081` | Port serving `/metrics`. |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn`, `error`. |
| `LOG_FORMAT` | `json` | `json` or `text`. |

## Conclusion

Only `SLACK_BOT_TOKEN` and `SLACK_DEFAULT_CHANNEL` are required. `SLACK_APP_TOKEN` turns mention invocation on, `CHAT_ACCESS_POLICIES` moves mention access into `AgentAccessPolicy` objects, and the analysis policy variables bound which alerts are analysed and how much model concurrency they may use.
