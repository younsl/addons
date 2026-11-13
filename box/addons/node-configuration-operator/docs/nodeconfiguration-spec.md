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
| `spec.rules` | required | Named rules, see [Rules](#rules) |

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

`spec.rules` is a flat list. Every rule has a `name` and exactly one action: `fileMode`, `loginShell`, `lineEdit`, `file`, or `script`. The rules run in list order on every run, and a failed rule does not stop the ones after it.

```yaml
rules:
  - name: drop-setuid
    fileMode:
      paths: [/usr/bin/newgrp, /usr/sbin/unix_chkpwd]
      clear: "4000"
  - name: deny-root-login
    lineEdit:
      path: /etc/ssh/sshd_config
      pattern: '^#?PermitRootLogin .*'
      replacement: PermitRootLogin no
```

The name is a lowercase DNS label of at most 63 characters, unique inside the resource. It identifies the rule in the agent log and in the status of a node that fails it, such as `rule deny-root-login failed, lineEdit /etc/ssh/sshd_config, permission denied`. A list holds at most 256 rules.

## fileMode

Sets or clears permission bits of existing files, like `chmod`. A path that does not exist is skipped.

| Field | Description |
| --- | --- |
| `paths` | Absolute paths. The last segment may hold `*` and `?` |
| `mode` | The exact octal mode to set, such as `0640` or `4750` |
| `clear` | Octal bits to remove from the current mode |

Exactly one of `mode` and `clear` is required.

| Shell | Rule |
| --- | --- |
| `chmod 640 /root/.bash*` | `mode: "0640"` |
| `chmod u-s /usr/bin/newgrp` | `clear: "4000"` |
| `chmod o-x /usr/bin/gcc` | `clear: "0001"` |
| `chmod 4750 /usr/bin/su` | `mode: "4750"` |

## loginShell

Sets the login shell of local accounts in `/etc/passwd`, like `usermod -s`. An account that does not exist is skipped.

| Field | Description |
| --- | --- |
| `users` | Account names |
| `shell` | The shell to set, such as `/sbin/nologin` |

## lineEdit

Replaces the first match of a regular expression on every line of an existing file, like `sed -i 's/pattern/replacement/'`. A file that does not exist is skipped.

| Field | Description |
| --- | --- |
| `path` | Absolute path of the file |
| `pattern` | A regular expression in the [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax), matched against each line without its line break |
| `replacement` | The replacement. `$1` or `${1}` insert a capture group |

Write the pattern so that the replaced line still matches or no longer needs a change. `^#?PermitRootLogin .*` replaced by `PermitRootLogin no` is stable, because the second run produces the same line.

Two differences from sed: a capture group is `$1` instead of `\1`, and the syntax has no backreferences or lookaround.

## file

Writes a file with exactly the given content. Whatever is at the path is replaced, a symbolic link included, which matches `rm -f` followed by a write. The directory must exist.

| Field | Default | Description |
| --- | --- | --- |
| `path` | | Absolute path of the file |
| `content` | | The whole file content, up to 64 KiB |
| `mode` | `0644` | The octal mode of the file |

## script

Runs a shell script as root inside the host root filesystem, through the `/bin/sh` of the node. Use it for what the other actions cannot express, such as reloading a service. It runs only when the chart value `agent.allowScripts` is on. Otherwise the rule fails and the other rules still run.

| Field | Default | Description |
| --- | --- | --- |
| `run` | | The script. A non-zero exit status fails the rule |
| `unless` | none | A check that must not change the node. Exit status 0 means the node is in line and `run` is skipped |
| `timeoutSeconds` | `60` | Seconds each of `unless` and `run` may take, 1 to 600 |

```yaml
rules:
  - name: deny-root-login
    lineEdit:
      path: /etc/ssh/sshd_config
      pattern: '^#?PermitRootLogin .*'
      replacement: PermitRootLogin no
  - name: reload-sshd
    script:
      unless: test /run/node-configuration/sshd-reloaded -nt /etc/ssh/sshd_config
      run: systemctl reload sshd && mkdir -p /run/node-configuration && touch /run/node-configuration/sshd-reloaded
```

The agent cannot see what a script changed, so a script gives up what the declarative actions provide.

- Without `unless` the script runs on every interval and is reported as `changed` every time. Write `unless` so a configured node is left alone.
- A dry run executes `unless` only. A failing check is reported as `pending`, and a script without `unless` is always `pending`.
- The schema cannot check the script. A typo shows up as a failed rule on the node.
- The script depends on the shell and tools of the node OS.
- The script runs in the agent container, so the memory limit of the agent applies to it. Standard output is dropped, and the end of the error output is reported when the script fails.
- When the timeout hits, the shell is killed. Processes it started in the background are not.

## Validation

The CRD schema rejects wrong types, relative paths, wildcards outside the last path segment, and a rule with no action or more than one, an empty script, a duplicate rule name, and a fileMode with both or neither of `mode` and `clear`. An invalid regular expression, an invalid `maxUnavailable`, and rules larger than 900 KiB cannot be expressed in the schema, so they surface as the `Ready` and `Progressing` conditions with the reason `InvalidSpec`.

## Conclusion

A NodeConfiguration is a target, a rollout strategy, and a flat list of named rules with one idempotent action each. Prefer the declarative actions, keep `script` for what they cannot express, and try a change with `dryRun` before it writes to a node.
