# Security

## What the agent can do

The agent pod has write access to the whole host filesystem. That is the purpose of the operator, and it makes the agent as sensitive as node root.

| Setting | Value |
| --- | --- |
| User | root (uid 0) |
| Privileged | false |
| Capabilities | `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, everything else dropped |
| Privilege escalation | not allowed |
| Container root filesystem | read-only |
| Seccomp | `RuntimeDefault` |
| Host namespaces | none (no hostPID, hostNetwork, or hostIPC) |
| Volume | hostPath `/` mounted at `/host` |
| Kubernetes API | Read NodeConfigurationBindings, write their status |

The three capabilities are what the rules need: `FOWNER` to change the mode of files owned by other accounts, `DAC_OVERRIDE` to enter home directories closed to root's group, and `CHOWN` to keep the owner of a rewritten file.

## With scripts allowed

`agent.allowScripts` trades the settings above for a privileged container with the host PID namespace, because a script needs whatever its commands need. The agent starts itself again as a helper that changes its root to `/host` and replaces itself with the host `/bin/sh`. Leave it off unless a rule needs it.

## What a resource author can do

Anyone who can create or update a NodeConfiguration can write any file on every selected node, including `/etc/sudoers` and files under `/etc/ssh`. Treat write access to the resource as node root on the managed node groups, and grant it as narrowly as cluster-admin.

The controller creates the agent DaemonSet, so its ServiceAccount can create DaemonSets in the release namespace and patch or delete the one named after the agent. It can read only the template ConfigMap. Whoever can edit that ConfigMap decides what the agent pods run.

The same holds for write access to NodeConfigurationBindings, because a binding is what the agent applies. Only the controller needs it. The agent can read bindings and write their status, and holds no AWS permission.

## Paths other accounts can write to

A rule may point into a directory a non-root account owns, such as a home directory. The agent runs as root, so it guards against a link planted there.

- A symbolic link is followed only when root created it. A link owned by another account fails the rule instead of redirecting it.
- A mode is changed on the opened file after it is checked to be the inspected entry and not a link.
- A rewrite goes to a new temporary file with an unpredictable name, created exclusively with mode `0600`, and never through an existing name.

A race on a parent directory that the account can rename is not closed. Prefer rules on root-owned paths, and treat rules under a home directory as best effort.

## What the operator does not do

- It never executes a command on the host unless `agent.allowScripts` is on. There is no shell in the image, and a script runs through the shell of the node.
- It never follows a symbolic link out of the host mount.
- It never selects a node that Karpenter owns.
