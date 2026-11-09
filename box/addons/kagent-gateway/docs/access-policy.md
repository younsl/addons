# Agent access policy

This guide shows how to decide, per agent name, who may invoke it through a mention and which kagent Agents answer, using the `AgentAccessPolicy` custom resource. It is for operators writing policies by hand or through the chart. The design rationale is in [designs/agent-access-policy.md](designs/agent-access-policy.md).

## Policy

`CHAT_ALLOWED_USERS` applies to every agent. The chart does not expose it as a value: allow lists live in `agentAccessPolicies`, and a deployment that still wants a global list sets `CHAT_ALLOWED_USERS` through `extraEnv`. To give an agent name its own allow list, set `CHAT_ACCESS_POLICIES=true` (chart: `chat.accessPolicies.enabled`) and create an `AgentAccessPolicy` in `KAGENT_NAMESPACE`. The policy name is the agent name `CHAT_AGENT` and `CHAT_AGENT_MAP` route to, and `agentRefs` lists the Agents that answer it:

```yaml
apiVersion: access.kagent-gateway.sh/v1alpha1
kind: AgentAccessPolicy
metadata:
  name: security-alert-triage-agent
  namespace: kagent
spec:
  agentRefs:
    - group: kagent.dev
      kind: Agent
      name: security-alert-triage-agent
      namespace: kagent
  chat:
    allowedUsers:
      - U01234567
```

## Weights

Like Gateway API `backendRefs`, each `agentRefs` entry carries a `weight` (default `1`), and a new thread goes to one Agent drawn by weight. A thread stays on the Agent it started on while that Agent keeps a weight above `0`, because its A2A `contextId` belongs to that Agent. Setting a weight to `0` drains the Agent: open threads move to a fresh session on another Agent at their next mention. This is how a new Agent version takes a share of questions before it takes all of them:

```yaml
spec:
  agentRefs:
    - {group: kagent.dev, kind: Agent, name: aws-alert-triage-agent, namespace: kagent, weight: 90}
    - {group: kagent.dev, kind: Agent, name: aws-alert-triage-agent-v2, namespace: kagent, weight: 10}
```

Weights apply to mentions only. The alert path keeps `KAGENT_AGENT_ROUTING_MAP`.

## Chart values

With the chart, list the policies under `agentAccessPolicies`, keyed by agent name. Each entry's `spec` is the policy spec verbatim, and the chart renders it into `kagent.namespace` unless the entry sets `enabled: false`. `weight` may be omitted (the CRD defaults it to 1), but writing it out keeps the split readable:

```yaml
chat:
  accessPolicies:
    enabled: true
agentAccessPolicies:
  security-alert-triage-agent:
    enabled: true
    spec:
      agentRefs:
        - group: kagent.dev
          kind: Agent
          name: security-alert-triage-agent
          namespace: kagent
          weight: 1
      chat:
        allowedUsers:
          - U01234567
```

## Decision rules

The check runs against the agent the channel routes to (`CHAT_AGENT_MAP`, then `CHAT_AGENT`), so one policy covers every channel that reaches the agent.

| Situation | Who may invoke the agent |
|-----------|--------------------------|
| Agent has no policy | `CHAT_ALLOWED_USERS` (empty, the chart default, allows everyone). Route a name only after its policy exists. |
| `allowedUsers` set | Only those members. The global list does not apply. |
| `allowAll: true` | Everyone in the allowed channels, even when `CHAT_ALLOWED_USERS` is restrictive |
| An `agentRefs` namespace differs from `KAGENT_NAMESPACE` | Nobody (`Ready=False`, reason `NamespaceMismatch`) |
| Every `agentRefs` weight is `0` | Nobody (`Ready=False`, reason `NoWeight`) |
| Policy cache not synced yet, or no Kubernetes client | Nobody, every mention is dropped as `policy_unsynced` |

The schema enforces the rest: every `agentRefs` entry names a `kagent.dev` `Agent` once (1 to 16 entries, weight 0 to 1,000,000), at least one weight is above `0`, and exactly one of a non-empty `allowedUsers` and `allowAll: true` is set, so an empty list is never read as "everyone". An agent name has at most one policy because the policy is named after it.

## Status and permissions

The gateway watches policies, so a change applies without a restart. Each policy reports `Ready` (enforced as written) and `AgentResolved` (every referenced Agent exists, re-checked every 5 minutes) in its status. A refused mention gets no reply and no hint, and `kagent_gateway_chat_access_denials_total{agent,reason}` counts it.

```bash
kubectl get agentaccesspolicies -n kagent
```

Enabling the feature creates a Role and RoleBinding in `kagent.namespace` (read policies, patch their status, get Agents) and mounts the ServiceAccount token into the Pod. The CRD ships with the chart (`crds.install`) and is kept on uninstall (`crds.keep`). The design is written up in [docs/designs/agent-access-policy.md](designs/agent-access-policy.md).

## Conclusion

Each routed agent name gets one validated, watched policy that names its Slack members and splits new threads across Agents by weight. Changes apply without a restart, mistakes are rejected by the API server or the chart schema, and every failure path refuses mentions while alert analysis keeps running. Create a policy for every routed name before routing to it, because a name without one falls back to `CHAT_ALLOWED_USERS`.
