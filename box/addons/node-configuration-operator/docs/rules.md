# Rules

A rule is one named action on the node OS. `spec.rules` of a [NodeConfiguration](nodeconfiguration-spec.md) is a flat list of them, and five actions are available.

| Action | Does | Like |
| --- | --- | --- |
| [fileMode](#filemode) | Sets or clears permission bits of existing files | `chmod` |
| [loginShell](#loginshell) | Sets the login shell of local accounts | `usermod -s` |
| [lineEdit](#lineedit) | Replaces a pattern on every line of an existing file | `sed -i` |
| [file](#file) | Writes a file with exactly the given content | `cat > file` |
| [script](#script) | Runs a shell script on the host, off by default | `sh -c` |

The first four are declarative: the agent compares before it writes, so a node already in line is left untouched and a dry run can report the difference. Each is implemented in Rust against the host mount, because the image holds no shell or tools.

## Anatomy

Every rule has a `name`, exactly one action (`fileMode`, `loginShell`, `lineEdit`, `file`, or `script`), and an optional [failurePolicy](#failure-policy). The rules run in list order on every run, and a failed rule does not stop the ones after it.

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

## Outcomes

Every rule reports one outcome per node and run, see [Status](how-it-works.md#status).

- `unchanged`: the node was already in line.
- `changed`: the rule corrected the node in this run.
- `pending`: a dry run found a difference and left it.
- `skipped`: the target does not exist on the node, such as a missing file or account.
- `failed`: the rule could not be applied. The agent retries after 60 seconds, unless the failure is ignored.

## Failure policy

`failurePolicy` says what a failure of the rule means for the node, with the same two values as the failure policy of a Kubernetes admission webhook or a Kyverno policy.

| Value | A failed rule |
| --- | --- |
| `Fail` (default) | Leaves the node not configured. The rollout stops there, lower priorities of the group wait, and the agent retries after 60 seconds |
| `Ignore` | Is reported and nothing else. The node counts as configured, the rollout moves on, and the rule runs again on the next interval |

```yaml
rules:
  - name: deny-root-login
    lineEdit:
      path: /etc/ssh/sshd_config
      pattern: '^#?PermitRootLogin .*'
      replacement: PermitRootLogin no
  - name: reload-sshd
    failurePolicy: Ignore
    script:
      run: systemctl reload sshd
```

Use `Ignore` for a rule that is nice to have, such as a tuning step that not every AMI supports. An ignored failure stays visible:

- The rule keeps the outcome `failed` in the binding and in `status.rules` of the NodeConfiguration.
- The binding stays `ready` with `failedRules: 0`, and its message reads `rule reload-sshd failed and was ignored, script run, exit code 1`. A failure that is not ignored takes the message first.
- The agent logs `Rule failed, ignored` as a warning.
- `failedNodes` and the metric `node_configuration_agent_last_run_failed_rules` leave it out. The counter `node_configuration_agent_rule_outcomes_total` counts it under `failed`.

A rule the schema or the controller rejects, such as an invalid regular expression, is an invalid spec whatever its policy. A missing target is `skipped` and never a failure.

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

## Conclusion

Four declarative actions cover permissions, accounts, line edits, and whole files, and each one is safe to repeat on every interval. Reach for `script` only when none of them fits, and give it an `unless` check so it stays as quiet as the others.
