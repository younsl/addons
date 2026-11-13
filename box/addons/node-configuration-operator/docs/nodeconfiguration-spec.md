# NodeConfiguration spec

NodeConfiguration is cluster-scoped, in the API group `node-configuration-operator.sh/v1alpha1`, with the short name `nc`. The name holds at most 63 characters.

## Fields

| Field | Default | Description |
| --- | --- | --- |
| `spec.target.nodeGroups` | all | Managed node group names. Empty selects every managed node group |
| `spec.target.nodeSelector` | none | Node labels a node must also carry |
| `spec.target.excludeNodes` | none | Node names to leave out |
| `spec.group` | none | Name of a set of resources that apply in order on a node |
| `spec.priority` | `0` | Order inside the group, highest first |
| `spec.dryRun` | `false` | Reports what a run would change without writing |
| `spec.intervalSeconds` | `300` | Seconds between two runs on a node, 30 to 86400 |
| `spec.strategy` | `RollingUpdate`, one node at a time | How a new revision replaces the previous one, see [Rollout](rollout.md) |
| `spec.minReadySeconds` | `0` | Seconds a node must stay configured before the rollout moves on |
| `spec.paused` | `false` | Holds a new revision back |
| `spec.progressDeadlineSeconds` | `600` | Seconds without progress before the rollout is reported as stalled |
| `spec.rules` | required | Named rules, see [Rules](rules.md) |

Karpenter nodes are never selected, whatever the target says. The controller derives the node affinity of the agent DaemonSet from the targets. Resources, tolerations, and the other pod settings of the agent are chart values, because one agent pod per node serves every NodeConfiguration.

## Groups and priority

Resources that share `spec.group` apply in order on each node. The one with the highest `spec.priority` runs first. A lower one runs on a node only after every higher one of the group is configured there, so a failed rule in an early resource holds the later ones back on that node. Resources of equal priority do not wait for each other, and a resource without a group runs independently.

```yaml
apiVersion: node-configuration-operator.sh/v1alpha1
kind: NodeConfiguration
metadata:
  name: account-hardening
spec:
  group: os-vulnerability-hardening
  priority: 200
  rules:
    - name: disable-system-logins
      loginShell:
        users: [sync, shutdown, halt]
        shell: /sbin/nologin
---
apiVersion: node-configuration-operator.sh/v1alpha1
kind: NodeConfiguration
metadata:
  name: ssh-hardening
spec:
  group: os-vulnerability-hardening
  priority: 100
  rules:
    - name: deny-root-login
      lineEdit:
        path: /etc/ssh/sshd_config
        pattern: '^#?PermitRootLogin .*'
        replacement: PermitRootLogin no
```

The order is kept per node, not across the cluster. Each resource still rolls out under its own strategy, and a binding that waits reports `waiting for <name>` and counts as unavailable in its own rollout.

## Dry run

With `spec.dryRun: true` the agent compares every rule and writes nothing. Each difference is reported with the outcome `pending`, and the binding message reads `dry run, 3 changes pending`. The node counts as configured, so a dry run rolls out to every node and does not hold a group back. Setting it back to false is a new revision that rolls out under the strategy.

## Coming from an SSM script

| Script | NodeConfiguration |
| --- | --- |
| Instances picked by a Name tag pattern | `spec.target.nodeGroups` or `spec.target.nodeSelector` |
| Unchecking a node before the run | `spec.target.excludeNodes` |
| `--dry-run` | `spec.dryRun` |
| Confirmation prompt | `spec.paused`, or `strategy.type: OnDelete` |
| Batch of 50 instances | `strategy.rollingUpdate.maxUnavailable` |
| Success and failure counts | `kubectl get nc` and `kubectl get ncb` |
| Rerun after a node is replaced | Not needed, a new node gets its binding when it joins |
| Region, profile, SSM permission | Not needed |

## Rules

`spec.rules` is a flat list of at most 256 rules. Every rule has a `name` and exactly one action: `fileMode`, `loginShell`, `lineEdit`, `file`, or `script`. The fields and behavior of each action are in [Rules](rules.md).

## Validation

The CRD schema rejects wrong types, relative paths, wildcards outside the last path segment, and a rule with no action or more than one, an empty script, a duplicate rule name, and a fileMode with both or neither of `mode` and `clear`. An invalid regular expression, an invalid `maxUnavailable`, and rules larger than 900 KiB cannot be expressed in the schema, so they surface as the `Ready` and `Progressing` conditions with the reason `InvalidSpec`.

## Conclusion

A NodeConfiguration is a target, a rollout strategy, and a flat list of named rules with one idempotent action each. Prefer the declarative actions, keep `script` for what they cannot express, and try a change with `dryRun` before it writes to a node.
