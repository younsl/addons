# Agent access policy

| Status | Category |
| --- | --- |
| implemented in 0.3.0 | access control |

## Summary

Let each agent name carry its own list of Slack members who may invoke it through a mention, and the set of kagent Agents that answer it. Both live in an `AgentAccessPolicy` custom resource, the gateway watches it, and the mention path checks the asker against the policy of the agent name the channel routes to, then sends the turn to one of the policy's `agentRefs`, picked by weight like Gateway API `backendRefs`. A name without a policy keeps the global `CHAT_ALLOWED_USERS` and is answered by the Agent of that name, so existing deployments behave exactly as before.

## Goals

- Restrict a sensitive agent (security, production access) to the people who own it, without restricting every other agent the same way.
- Key the rule by agent, not by channel, so one policy covers every channel that reaches the agent and adding a channel never opens a gap.
- Apply a change without restarting the pod, because a restart drops the Socket Mode connection and in-memory thread sessions.
- Let the team that owns an agent edit its policy through namespace RBAC and GitOps, without access to the gateway's values.
- Make the effective state visible on the object itself (`kubectl get aap`).
- Let a new Agent version take a share of new threads before it takes all of them, and drain the old one without breaking threads in progress.

## Non-goals

- Access control and weights on the alert path. Alerts are not invoked by a person, and the alert path keeps `KAGENT_AGENT_ROUTING_MAP`.
- Slack user groups (`S...`) as list entries. See [Open questions](#open-questions).
- Moving channel routing (`CHAT_AGENT_MAP`) into the custom resource.
- Policies for agents outside `KAGENT_NAMESPACE`. The gateway only submits there.
- Multi replica status writes. The chart runs one replica.

## Background

Before this change the mention path had one user allow list, `CHAT_ALLOWED_USERS`, checked before the agent was chosen. A deployment that routes `#security-alerts` to a security agent and `#infra-alerts` to an AWS agent could not say "only the security team may ask the security agent". The only lever was the channel allow list, which follows Slack channel membership rather than who should spend that agent's tools and tokens.

An env-only design (`CHAT_AGENT_ALLOWED_USERS=agent=U1|U2,...`) was considered first. It was rejected because every edit needs a rollout, the gateway's values owner becomes the gatekeeper for every agent team, and a typo in an agent key silently leaves that agent on the global list.

## Architecture

![Agent access policy](agent-access-policy.svg)

The gateway runs a controller over `AgentAccessPolicy` objects in `KAGENT_NAMESPACE`. It reads them from its reflector cache on every mention and writes back only their status. Each policy points at one or more kagent `Agent`s through `agentRefs`, and a mention reaches one of them over A2A only when the asker passes the policy.

Order of checks on one mention:

```
Slack app_mention
  -> drop rules (bot, subtype, dm)
  -> resolve agent from channel (CHAT_AGENT_MAP, then CHAT_AGENT)
  -> access check against that agent's policy, or CHAT_ALLOWED_USERS
  -> thread, duplicate, channel, and empty checks
  -> backend: the thread's Agent if it still has weight, else a weighted pick
  -> agent turn
```

The agent is resolved before the thread check, so a refused user gets no thread hint either. That keeps the old behaviour where a user outside the allow list learns nothing from the bot.

Code layout:

| Path | Responsibility |
|------|----------------|
| `src/access.rs` | The decision: `Rule`, `Policy`, `Backend`, `Lookup`, `PolicySource`, `check()`, and the weighted `pick()`. No Kubernetes types. |
| `src/crd.rs` | The `AgentAccessPolicy` type, its schema, and its CEL rules |
| `src/crd/chart.rs` | Renders the CRD into the chart. `make crd` writes it, `tests/crd_drift.rs` fails when it is stale. |
| `src/k8s/policy.rs` | `kube::runtime::Controller`, the reflector cache the mention path reads, and the status conditions |
| `src/bin/crdgen.rs` | Build tool behind `make crd`. Never shipped in the image. |

The mention path reads the reflector `Store` in memory, so a mention costs no API call.

## Custom resource

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
      weight: 90
    - group: kagent.dev
      kind: Agent
      name: security-alert-triage-agent-v2
      namespace: kagent
      weight: 10
  chat:
    allowedUsers:
      - U01234567
    # allowAll: true
status:
  observedGeneration: 1
  conditions:
    - type: AgentResolved
      status: "True"
      reason: Found
    - type: Ready
      status: "True"
      reason: Enforced
```

### Validation

Every rule a single object can express is enforced by the API server through the schema and `x-kubernetes-validations`. There is no admission webhook.

| Rule | Why |
|------|-----|
| `agentRefs` entries name `kagent.dev` / `Agent` | Each reference sets every identifying field, and only Agents are governed. |
| 1 to 16 `agentRefs`, each Agent named once | A duplicate would silently add up two weights for one Agent. |
| `weight` 0 to 1,000,000, default 1, at least one above 0 | The same range and default as Gateway API `backendRefs`. A policy with no share anywhere has nowhere to send a turn. |
| Exactly one of a non-empty `allowedUsers` and `allowAll: true` | An empty list can never be read as "everyone". Opening an agent is always explicit. |
| `allowedUsers` entries match `^[UW][A-Z0-9]+$`, at most 1000, each at most 32 characters | Catches display names and emails pasted in place of member IDs. The bounds keep the CEL cost estimate inside the API server budget. |

One policy per agent name needs no rule: the policy is named after the name it governs, and object names are unique in a namespace.

Every `agentRefs` namespace must equal `KAGENT_NAMESPACE`. CEL cannot read the object's own namespace, so the controller checks it and reports `Ready=False` with reason `NamespaceMismatch`.

### Status

| Condition | True | False | Unknown |
|-----------|------|-------|---------|
| `Ready` | `Enforced`: the policy applies as written | `NamespaceMismatch` or `NoWeight`: every mention to the name is denied | not used |
| `AgentResolved` | `Found`: every referenced Agent exists | `NotFound`: the message lists the missing Agents, and a thread picked for one fails until it is created | `LookupFailed`: discovery or a get failed |

`lastTransitionTime` moves only when a condition's status changes. Every policy is requeued every 5 minutes, which is what flips `AgentResolved` when an Agent is created after its policy. The Agent API version comes from discovery, because kagent has served `Agent` under more than one version.

## Decision rules

| Situation | Who may invoke the agent | Drop reason |
|-----------|--------------------------|-------------|
| Feature off (`CHAT_ACCESS_POLICIES=false`) | `CHAT_ALLOWED_USERS` | `user_denied` |
| Agent has no policy | `CHAT_ALLOWED_USERS` (empty allows everyone) | `user_denied` |
| `allowedUsers` set | Only those members. The global list does not apply. | `user_denied` |
| `allowAll: true` | Everyone in the allowed channels | none |
| `NamespaceMismatch` or `NoWeight` | Nobody | `policy_invalid` |
| Cache not synced, or no Kubernetes client | Nobody | `policy_unsynced` |

A policy replaces the global list rather than intersecting with it. Replacement lets an agent be opened (`allowAll`) or narrowed independently of the default, which is the point of a per-agent rule. Intersection would make the global list a ceiling no agent could rise above.

An agent without a policy falls back to the global list rather than being denied. That keeps the feature safe to turn on in an existing deployment: nothing changes until the first policy is created.

A user refused for the routed agent is never retried against the default agent. Falling through would turn a policy into a suggestion.

## Weights

A turn that passes the access check goes to one of the policy's `agentRefs`:

- A thread that already holds a session stays on the Agent that session belongs to, as long as that Agent is still listed with a weight above `0`. A `contextId` means nothing to another Agent, so a split by turn would break every follow-up.
- Otherwise the gateway draws an Agent in proportion to the weights and the turn starts a fresh session. A weight of `0` is never drawn.

The session store therefore keeps the Agent next to the `contextId` for each thread. Lowering an Agent to `0` drains it: threads in progress move to a fresh session on another Agent at their next mention, and no new thread starts there. Removing an entry has the same effect.

The split is per thread, not per mention, and it is random rather than hashed. A thread is the unit a person experiences, and with sessions in memory at one replica there is nothing a hash would buy. `chat_turns_total{agent}` carries the Agent that answered, so the split is visible per Agent.

## Failure modes

- **Cache not yet synced:** the reflector has not finished its initial list, so a missing policy cannot be told apart from one not yet seen. Every mention is refused as `policy_unsynced` and logged at warn. `access_policies_synced` reads 0.
- **No Kubernetes client** (no ServiceAccount token, bad kubeconfig): the gateway does not exit. It installs a source that never syncs, so mentions fail closed and the alert path keeps working.
- **Watch broken after sync:** the cache keeps the last state it saw and the watcher retries with backoff. Policies stay enforced as last seen.
- **Missing RBAC:** the watch or status patch fails, `access_policy_reconciles_total{result="error"}` rises, and the cache never syncs, so mentions fail closed.
- **CRD not installed:** same as missing RBAC. The watch fails and mentions fail closed.

Readiness is not tied to the policy cache. A broken cache must not make the pod unready and stop alert analysis, the same reasoning that keeps the Socket Mode connection out of readiness.

## Configuration

| Variable | Chart value | Default | Effect |
|----------|-------------|---------|--------|
| `CHAT_ACCESS_POLICIES` | `chat.accessPolicies.enabled` | `false` | Starts the controller and reads policies from `KAGENT_NAMESPACE` |
| `CHAT_ALLOWED_USERS` | `chat.allowedUsers` | empty | With the feature on, applies only to agents without a policy |
| none | `agentAccessPolicies` | `{}` | Policies rendered into `kagent.namespace`, keyed by agent name. Each entry's `spec` is the policy spec verbatim, so the chart adds no defaults of its own and the CRD schema stays the single source of them. |
| none | `crds.install` | `true` | Installs and upgrades the CRD from `templates/crds/` |
| none | `crds.keep` | `true` | Keeps the CRD, and every policy, on `helm uninstall` |

Rendered policies carry `argocd.argoproj.io/sync-options: SkipDryRunOnMissingResource=true`, because on the first sync the CRD and the policies arrive together and the dry run cannot resolve the kind yet. With `crds.install: false` they render only when the API server already serves the kind. The values schema mirrors the CEL rules (member ID pattern, exactly one of the two fields), so a mistake fails `helm template` instead of the sync.

The CRD sits in `templates/` rather than `crds/` because Helm never upgrades anything in `crds/`, so a schema change there would silently not apply.

With the feature on, the chart adds:

- a Role and RoleBinding in `kagent.namespace`: `get`, `list`, `watch` on `agentaccesspolicies`, `patch` on `agentaccesspolicies/status`, and `get` on `kagent.dev` `agents`
- `automountServiceAccountToken: true` on the Pod. The ServiceAccount keeps its own setting.

## Observability

| Metric | Type | Description |
|--------|------|-------------|
| `kagent_gateway_chat_access_denials_total{agent,reason}` | Counter | Mentions refused by the access check, by the agent the channel routes to |
| `kagent_gateway_access_policies` | Gauge | Policies in the cache, refreshed every 30 seconds because deletions never reach the reconciler |
| `kagent_gateway_access_policies_synced` | Gauge | 1 once the initial list completed |
| `kagent_gateway_access_policy_reconciles_total{result}` | Counter | Reconciles, by `ok` or `error` |

`chat_events_total` gains the drop reasons `policy_unsynced` and `policy_invalid`. A refused mention still gets no reply and no hint, so `chat_access_denials_total` is what turns "the bot ignored me" into a config answer.

Startup logs one line listing the policies once synced, and the existing allow list line now says whether it applies to every agent or only to agents without a policy.

## Testing

- `access.rs`: fallback to the global list, replacement by a policy, `allowAll`, and both fail-closed cases.
- `k8s/policy.rs`: rule derivation, every condition and reason, `lastTransitionTime` held while the status holds, and the store answering `Unsynced` until ready.
- `gateway/chat.rs`: a policy on the routed agent replaces the global list while an unmapped channel keeps it, and an unsynced source refuses every mention.
- `tests/crd_drift.rs`: the chart CRD equals what the Rust types generate.
- Manual: apply the CRD to a cluster and confirm the API server rejects a name mismatch, an empty `chat`, both fields set, and a non-ID user entry. Not yet run.

## Rollout

1. Release with `chat.accessPolicies.enabled: false`. The CRD installs, nothing else changes.
2. Enable it. With no policies every agent stays on `chat.allowedUsers`. Confirm `access_policies_synced` is 1.
3. Create a policy for the most sensitive agent and confirm `Ready=True` and `AgentResolved=True`.
4. Move per-agent restrictions out of `chat.allowedUsers` into policies. Keep the global list as the default for agents without one.

## Open questions

- Slack user groups as entries (`S...`) would remove the per-person churn. It needs the `usergroups:read` scope, a cached `usergroups.users.list`, and a rule for a group that cannot be resolved (deny is the only safe answer).
- Weights on the alert path, which today routes by `KAGENT_AGENT_ROUTING_MAP` alone.
- A `CHAT_REQUIRE_POLICY` switch that denies agents without a policy, for deployments that want every agent covered explicitly.
- More than one replica needs leader election before status writes, or status writes moved to one replica.
- A denied-user hint, opt-in, for teams that prefer telling people why the bot is silent over not revealing the rule.

## Conclusion

Access is now decided per agent name by a validated, watched custom resource, with the global allow list as the fallback, and the same object splits new threads across Agent versions by weight. Turning the feature on changes nothing until a policy exists, every error path fails closed for mentions and leaves alerts alone, and the effective state of each policy is on the object itself.
