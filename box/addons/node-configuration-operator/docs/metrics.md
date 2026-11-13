# Metrics

The controller and every agent serve Prometheus metrics at `/metrics` on port 8080 (`http`), next to `/healthz` and `/readyz`. Both roles run the same image and register the same metric names. A role only writes the metrics that belong to it, so the others stay empty there.

## Overview

| Metric | Type | Labels | Role | Description |
| --- | --- | --- | --- | --- |
| `node_configuration_build_info` | info | `version`, `revision`, `rust_version` | both | Build information |
| `node_configuration_reconcile_total` | counter | `result` | both | Reconciles in the controller, runs in the agent |
| `node_configuration_controller_leader` | gauge | none | controller | 1 while this replica acts |
| `node_configuration_agent_rule_outcomes_total` | counter | `configuration`, `kind`, `outcome` | agent | Rule outcomes on this node |
| `node_configuration_agent_last_run_timestamp_seconds` | gauge | `configuration` | agent | Unix time of the last run |
| `node_configuration_agent_last_run_failed_rules` | gauge | `configuration` | agent | Rules that failed in the last run |

No metric carries a node label. An agent reports about the node it runs on, so the node comes from the scrape target, see [Scraping](#scraping).

## Reference

### node_configuration_build_info

Always 1. The labels identify the build that runs in the pod.

| Label | Value |
| --- | --- |
| `version` | Image version, such as `0.2.0` |
| `revision` | Git commit the image was built from |
| `rust_version` | Rust toolchain of the build |

Use it to see which version each pod runs during an upgrade: `count by (version) (node_configuration_build_info)`.

### node_configuration_reconcile_total

Counts finished passes by `result`, which is `success` or `error`.

| Role | One count is | `error` means |
| --- | --- | --- |
| Controller | One reconcile of one NodeConfiguration | A Kubernetes API request failed. The pass is retried after 30 seconds |
| Agent | One run of one binding | The run was aborted or the status could not be written. The run is retried after 60 seconds |

A rule that fails is not an `error` here. The run itself finished, and the failure is counted in `node_configuration_agent_rule_outcomes_total` and `node_configuration_agent_last_run_failed_rules`.

An agent counts a run only when it applies the rules: when the spec of a binding changes and then every `intervalSeconds`. With one binding and the default interval of 300 seconds that is 12 runs per hour.

### node_configuration_controller_leader

1 on the controller replica that rolls configurations out, 0 on a standby. Without leader election the single replica always reports 1. An agent always reports 0.

The sum over all controller pods is 1 in a healthy release. It is 0 for a moment during a handover and stays 0 when no replica can take the Lease.

### node_configuration_agent_rule_outcomes_total

Counts every rule of every run on this node. A rule that touches several paths counts once per path.

| Label | Value |
| --- | --- |
| `configuration` | Name of the NodeConfiguration |
| `kind` | Action of the rule: `fileMode`, `loginShell`, `lineEdit`, `file`, or `script` |
| `outcome` | Result of the rule in that run, see below |

| Outcome | Meaning |
| --- | --- |
| `unchanged` | The node was already in line |
| `changed` | The rule corrected the node |
| `pending` | A dry run found a difference and left it |
| `skipped` | The target does not exist on the node, such as a missing file or account |
| `failed` | The rule could not be applied |

How to read it:

- `changed` on the first run of a node is the hardening being applied. `changed` later on means the node drifted and was corrected. A steady rate points at something else that keeps rewriting the same file.
- `unchanged` grows on every run of a node that is in line. It is the heartbeat of a healthy rule.
- `pending` only appears with `spec.dryRun: true`.
- `skipped` is expected where a target is optional. It does not make a node unready.

The counter keeps what the status of a binding does not. The status holds the last run, and this counter holds every run since the agent started.

### node_configuration_agent_last_run_timestamp_seconds

Unix time at which the agent last ran a configuration on this node. It moves on every run, also when nothing changed, so it tells a working agent from a stuck one. The binding status is written only when it changes and cannot tell that.

A binding that waits for a higher priority of its group does not run, so the value stays where it was until the wait ends.

### node_configuration_agent_last_run_failed_rules

Number of rules that failed in the last run of a configuration on this node. 0 means the last run applied every rule. The value drops back to 0 as soon as a later run succeeds.

## Example output

```console
$ kubectl port-forward -n kube-system ds/node-configuration-operator-agent 8080:8080
$ curl -s localhost:8080/metrics | grep -v '^#'
node_configuration_build_info{version="0.2.0",revision="a5ac1432",rust_version="1.100.0"} 1
node_configuration_reconcile_total{result="success"} 12
node_configuration_agent_rule_outcomes_total{configuration="os-hardening",kind="fileMode",outcome="changed"} 19
node_configuration_agent_rule_outcomes_total{configuration="os-hardening",kind="fileMode",outcome="unchanged"} 209
node_configuration_agent_rule_outcomes_total{configuration="os-hardening",kind="fileMode",outcome="skipped"} 12
node_configuration_agent_last_run_timestamp_seconds{configuration="os-hardening"} 1794546585
node_configuration_agent_last_run_failed_rules{configuration="os-hardening"} 0
node_configuration_controller_leader 0
```

## Scraping

The chart creates no Service and no monitor resource. Scrape the pods directly. With the Prometheus Operator one PodMonitor covers both roles, and a relabeling copies the node name onto the agent series.

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PodMonitor
metadata:
  name: node-configuration-operator
  namespace: kube-system
spec:
  namespaceSelector:
    matchNames:
      - kube-system
  selector:
    matchExpressions:
      - key: app.kubernetes.io/name
        operator: In
        values:
          - node-configuration-operator
          - node-configuration-operator-agent
  podMetricsEndpoints:
    - port: http
      path: /metrics
      relabelings:
        - sourceLabels: [__meta_kubernetes_pod_node_name]
          targetLabel: node
```

The chart value `extraObjects` can ship this PodMonitor with the release. For annotation-based discovery set `podAnnotations` and `agent.podAnnotations` instead.

## Queries

The queries below assume the `node` label from the PodMonitor above.

| Question | Expression |
| --- | --- |
| Which nodes have failing rules | `node_configuration_agent_last_run_failed_rules > 0` |
| Seconds since the last run per node | `time() - node_configuration_agent_last_run_timestamp_seconds` |
| What was corrected in the last day, per node | `sum by (node, configuration, kind) (increase(node_configuration_agent_rule_outcomes_total{outcome="changed"}[1d])) > 0` |
| Differences a dry run still sees | `sum by (configuration) (increase(node_configuration_agent_rule_outcomes_total{outcome="pending"}[10m]))` |
| Controller error ratio | `sum(rate(node_configuration_reconcile_total{result="error"}[5m])) / sum(rate(node_configuration_reconcile_total[5m]))` |
| Which pod leads | `node_configuration_controller_leader == 1` |
| Versions in the cluster | `count by (version) (node_configuration_build_info)` |

Rollout state is in the status of the resources, not in a metric. Watch the `Progressing` condition for `ProgressDeadlineExceeded`, see [Rollout](rollout.md#watching-a-rollout).

## Alert ideas

| Condition | Expression | Note |
| --- | --- | --- |
| No controller replica leads | `sum(node_configuration_controller_leader) < 1` | Hold it for a few minutes so a handover does not fire |
| A node has failing rules | `node_configuration_agent_last_run_failed_rules > 0` | The agent retries a failed rule every 60 seconds |
| An agent stopped running | `time() - node_configuration_agent_last_run_timestamp_seconds > 3600` | Set the threshold to a few times the longest `intervalSeconds` |
| Repeated drift on a node | `increase(node_configuration_agent_rule_outcomes_total{outcome="changed"}[1h]) > 5` | Something else rewrites what a rule manages |
| The controller keeps failing | `increase(node_configuration_reconcile_total{result="error"}[15m]) > 5` | Usually an RBAC or API server problem |
