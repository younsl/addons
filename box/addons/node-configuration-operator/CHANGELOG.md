# Changelog

Notable changes to the node-configuration-operator (NCO) app (the container image), newest first. Entries are headed by the image version and its release date (KST).

## 0.2.0 (2026-11-13)

### Added

- `spec.strategy.rollingUpdate.cordon` (default `false`). The controller cordons a node right before it moves it to a new revision and uncordons it once the node is configured for `minReadySeconds`, so no new Pod is scheduled on a node whose configuration is changing. A node that was already cordoned is left as it is, a dry run never cordons, and the node is released when it leaves the target, when `cordon` is turned off, and when the NodeConfiguration is deleted. See [docs/rollout.md](docs/rollout.md#cordon).
- `Cordon` and `Uncordon` Events on the Node, each with a sentence that names the NodeConfiguration and the revision or the cause, shown by `kubectl describe node`.
- Leader election for the controller through a Lease, with the flags `--leader-election`, `--leader-election-lease-name`, `--leader-election-lease-duration-seconds` (`15`), `--leader-election-renew-deadline-seconds` (`10`), `--leader-election-retry-period-seconds` (`2`), and `--pod-name`. A standby stays ready and idle, a leader that stops gives the Lease back, and a leader that cannot renew in time stops before a standby may take over. See [docs/installation.md](docs/installation.md#high-availability).
- Metric `node_configuration_controller_leader`, 1 on the replica that acts.
- Binding status fields `lastChangeTime` and `lastChangedRules`, and per rule `lastChangeTime` and `lastChange`. They keep what a run corrected after the following runs report `unchanged`, so the first run on a new node stays visible in the resource.
- `STATUS AGE`, `LAST CHANGE`, and (wide) `CHANGED` columns in `kubectl get ncb`.

### Changed

- A NodeConfigurationBinding is owned by its Node instead of its NodeConfiguration, like a Karpenter NodeClaim. The Kubernetes garbage collector removes it with the node, also while no controller runs, and a node replaced under the same name gets a fresh binding. The controller removes the bindings of a deleted NodeConfiguration. Bindings of 0.1.0 are handed over on the first pass. See [docs/how-it-works.md](docs/how-it-works.md#binding-lifecycle).
- The controller ClusterRole patches Nodes and creates Events, no longer needs `nodeconfigurations/finalizers`, and the Role manages the leader Lease. Chart 0.2.0 ships the rules.

### Fixed

- The agent ran the rules of a binding again right after every status it wrote, because the write came back as a watch event. A new node ran three times within seconds and its `changed` outcomes were overwritten by `unchanged` at once. A binding now runs when its spec changes and then every `intervalSeconds`.

## 0.1.0 (2026-11-13)

First release. A Kubernetes operator that keeps the OS configuration of EKS managed node group nodes in line with NodeConfiguration resources, replacing one-off hardening scripts run through SSM. One image runs as the controller and as the agent.

### Added

- `NodeConfiguration` custom resource (`node-configuration-operator.sh/v1alpha1`, cluster-scoped, short name `nc`). It declares the rules, the target nodes, and the rollout strategy. See [docs/nodeconfiguration-spec.md](docs/nodeconfiguration-spec.md).
- `spec.rules` is a flat list of named rules with exactly one action each, run in list order: `fileMode` (like `chmod`, with wildcards in the last path segment), `loginShell` (like `usermod -s`), `lineEdit` (like `sed -i`, in the Rust regex syntax), and `file` (whole content, replacing a symbolic link). Every action compares before it writes, so a node already in line is left untouched.
- `script` action for what the declarative actions cannot express, such as reloading a service. It runs through the `/bin/sh` of the node, skips `run` when `unless` exits with 0, and is killed after `timeoutSeconds` (default `60`). It needs the chart value `agent.allowScripts`, which makes the agent pod privileged with the host PID namespace.
- Targeting by `spec.target.nodeGroups`, `nodeSelector`, and `excludeNodes`. Only nodes with the `eks.amazonaws.com/nodegroup` label are selected, and nodes with the `karpenter.sh/nodepool` label never are.
- `NodeConfigurationBinding` custom resource (short name `ncb`), one revision of one NodeConfiguration on one node. The controller writes it, and the agent on that node applies it every `spec.intervalSeconds` (default `300`) and reports the outcome of every rule in its status.
- Deployment-style rollout per NodeConfiguration: `strategy.type` (`RollingUpdate` or `OnDelete`), `rollingUpdate.maxUnavailable` (count or percentage, default `1`), `minReadySeconds`, `paused`, and `progressDeadlineSeconds` (default `600`). A node that fails a rule keeps its place in the budget, which stops the rollout there. See [docs/rollout.md](docs/rollout.md).
- One agent DaemonSet for the whole cluster, created by the controller from a template the chart ships. Its node affinity is the union of the targets of every NodeConfiguration, it is updated when a target changes, and it is removed when no NodeConfiguration is left. One agent pod per node applies every binding of that node.
- `spec.group` and `spec.priority`. Resources of one group apply in order on a node, highest priority first, and a lower one waits until every higher one is configured there.
- `spec.dryRun`. The agent compares every rule, writes nothing, and reports each difference as `pending`.
- Aggregated status on the NodeConfiguration: `failedNodes`, `rules` (per rule, how many nodes reported `unchanged`, `changed`, `pending`, `skipped`, or `failed`), and `failures` (up to ten nodes that are not configured and why), next to the `Ready` and `Progressing` conditions. `kubectl get nc` prints `DESIRED`, `UPDATED`, `CONFIGURED`, `FAILED`, `READY`, `ROLLOUT`, and `AGE`.
- Host access without SSM, SSH, or AWS permission. The agent mounts the host root at `/host`, resolves paths the way a process chrooted into the host would, and rewrites files through a temporary file and a rename that keep the mode and owner. It runs as root with the `CHOWN`, `DAC_OVERRIDE`, and `FOWNER` capabilities and is not privileged unless scripts are allowed. See [docs/security.md](docs/security.md).
- Guards for paths a non-root account can write to: a symbolic link is followed only when root created it, a mode is changed on the opened entry after it is checked not to be a link, and temporary files are created exclusively with an unpredictable name.
- Validation in the CRD schema and CEL rules. Checks the schema cannot express (an invalid regular expression, an invalid `maxUnavailable`, rules above 900 KiB) surface as the reason `InvalidSpec` on both conditions, and the nodes keep the last valid revision.
- Prometheus metrics `node_configuration_build_info`, `node_configuration_reconcile_total`, `node_configuration_agent_rule_outcomes_total`, `node_configuration_agent_last_run_timestamp_seconds`, and `node_configuration_agent_last_run_failed_rules`, served with `/healthz` and `/readyz` on port 8080. See [docs/metrics.md](docs/metrics.md).

### Known limits

- A removed rule is not undone. The node keeps the last applied state until it is replaced.
- There is no automatic rollback. A failed rule stops the rollout on that node.
- Node group names are matched exactly, not by pattern.
- A node named in `excludeNodes` still runs an idle agent pod when the rest of the target selects it.
