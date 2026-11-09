# Agent access policy

This guide shows how to route Slack mentions and alerts to kagent Agents and decide who may invoke them, all in `AgentAccessPolicy` objects. It is for operators writing policies by hand or through the chart. The design rationale is in [designs/agent-access-policy.md](designs/agent-access-policy.md).

## Policy

Set `CHAT_ACCESS_POLICIES=true` (chart: `chat.accessPolicies.enabled`) and every routing decision moves into `AgentAccessPolicy` objects in `KAGENT_NAMESPACE`, the way Gateway API keeps matches and backends in routes. One policy is one route:

```yaml
apiVersion: access.kagent-gateway.sh/v1alpha1
kind: AgentAccessPolicy
metadata:
  name: database
  namespace: kagent
spec:
  match:
    slackChannels: [prod-slowquery-alarm, devops-dba-alarm]
    alertLabel:
      name: slack_channel
      values: [prod-slowquery-alarm, devops-dba-alarm]
  alertChannel: devops-dba-alarm
  agentRefs:
    - group: kagent.dev
      kind: Agent
      name: database-alert-triage-agent
      namespace: kagent
      weight: 1
  chat:
    allowedUsers:
      - U01234567
```

- `match.slackChannels` takes mentions in these channels (names or conversation IDs).
- `match.alertLabel` takes alerts whose label carries one of these values.
- `alertChannel` is where those alerts thread. Without it, the matched label value names the channel.
- `agentRefs` are the Agents that answer.
- `chat` says who may mention: `allowedUsers` (Slack member IDs) or `allowAll: true`.

`CHAT_AGENT_MAP`, `CHAT_CHANNELS`, `KAGENT_AGENT_ROUTING_MAP`, `SLACK_CHANNEL_MAP`, and `CHAT_ALLOWED_USERS` are ignored while policies route. The gateway logs a warning at startup when any of them is still set.

## Matching

- A named channel or label value beats the `*` catch-all. `slackChannels: ["*"]` takes every channel no other policy names, and `alertLabel.values: ["*"]` every alert no other policy matches.
- Anything no policy matches is dropped, like a request no route matches: a mention as `channel_denied` (the asker gets the denied hint), an alert as the skip reason `unrouted`. A default is an explicit catch-all policy.
- When two policies claim the same channel or label value, the older one keeps it (then the name in alphabetical order). The newer one reports `Accepted=False` with reason `Conflicted`, and its other matches still apply.

## Weights

Like Gateway API `backendRefs`, each `agentRefs` entry carries a `weight` (default `1`). A new thread goes to one Agent drawn by weight, and stays on it while that Agent keeps a weight above `0`, because its A2A `contextId` belongs to that Agent. Setting a weight to `0` drains the Agent: open threads move to a fresh session on another Agent at their next mention. Alerts draw an Agent on every analysis, since each analysis is a new task. This is how a new Agent version takes a share before it takes everything:

```yaml
spec:
  agentRefs:
    - {group: kagent.dev, kind: Agent, name: aws-alert-triage-agent, namespace: kagent, weight: 90}
    - {group: kagent.dev, kind: Agent, name: aws-alert-triage-agent-v2, namespace: kagent, weight: 10}
```

## Chart values

With the chart, list the policies under `agentAccessPolicies`, keyed by name. Each entry's `spec` is the policy spec verbatim, and the chart renders it into `kagent.namespace` unless the entry sets `enabled: false`:

```yaml
chat:
  enabled: true
  accessPolicies:
    enabled: true
agentAccessPolicies:
  database:
    enabled: true
    spec:
      match:
        slackChannels: [devops-dba-alarm]
        alertLabel:
          name: slack_channel
          values: [devops-dba-alarm]
      agentRefs:
        - group: kagent.dev
          kind: Agent
          name: database-alert-triage-agent
          namespace: kagent
          weight: 1
      chat:
        allowedUsers:
          - U01234567
```

`weight` may be omitted (the CRD defaults it to 1), but writing it out keeps the split readable. An entry with `enabled: false` is not rendered, so its channels and alerts are dropped unless another policy matches them.

## Decision rules

| Situation | Outcome |
|-----------|---------|
| Matched, user in `allowedUsers` or `allowAll: true` | Answered by an Agent from `agentRefs` |
| Matched, user not in `allowedUsers` | Refused as `user_denied`, no reply and no hint |
| No policy matches the channel | Refused as `channel_denied`, with the denied hint |
| No policy matches the alert | Not analysed, skip reason `unrouted` |
| An `agentRefs` namespace differs from `KAGENT_NAMESPACE`, or every weight is `0` | Refused (`policy_invalid`), `Ready=False` with reason `NamespaceMismatch` or `NoWeight` |
| Policy cache not synced yet, or no Kubernetes client | Mentions refused and alerts skipped as `policy_unsynced`. Alertmanager's next resend is analysed. |

## Validation

The API server validates every policy through the CRD schema and CEL rules, with no admission webhook. A mistake fails at `kubectl apply` or Argo CD sync:

- `match` names at least one channel or an `alertLabel`, entries do not repeat, carry no spaces, and `*` stands alone.
- `alertChannel` is one channel and needs `match.alertLabel`.
- `agentRefs` has 1 to 16 entries naming `kagent.dev` `Agent`s once, in one namespace, with weights 0 to 1,000,000 and at least one above `0`.
- `chat` sets exactly one of a non-empty `allowedUsers` (Slack member IDs only) and `allowAll: true`, so an empty list is never read as "everyone".

What one object cannot tell, such as two policies claiming one channel or an `agentRefs` namespace other than `KAGENT_NAMESPACE`, surfaces as a status condition instead. The chart values schema mirrors the same rules, so a mistake in values fails `helm template` first.

## Status and permissions

The gateway watches policies, so a change applies without a restart. Each policy reports `Accepted` (it holds every match it claims), `Ready` (enforced as written), and `AgentResolved` (every referenced Agent exists, re-checked every 5 minutes) in its status. `kagent_gateway_chat_access_denials_total{agent,reason}` counts refused mentions by policy, and `kagent_gateway_analyses_skipped_total{reason}` counts dropped alerts.

```bash
kubectl get agentaccesspolicies -n kagent
```

Enabling the feature creates a Role and RoleBinding in `kagent.namespace` (read policies, patch their status, get Agents) and mounts the ServiceAccount token into the Pod. The CRD ships with the chart (`crds.install`) and is kept on uninstall (`crds.keep`).

## Conclusion

With access policies on, each `AgentAccessPolicy` is one route: what it matches, where alerts thread, which Agents answer by weight, and who may ask. Routing lives in one validated, watched place instead of several env tables, single-object mistakes are rejected by CEL at apply time, conflicts show as `Accepted=False`, and anything no policy matches is dropped. Add a `*` catch-all policy when a default is wanted.
