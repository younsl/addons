# Metrics

The controller and every agent serve `/metrics` on port 8080 (`http`), next to `/healthz` and `/readyz`.

| Metric | Type | Labels | Role | Description |
| --- | --- | --- | --- | --- |
| `node_configuration_build_info` | info | `version`, `revision`, `rust_version` | both | Build information |
| `node_configuration_reconcile_total` | counter | `result` | both | Reconciles in the controller, runs in the agent, by `success` or `error` |
| `node_configuration_agent_rule_outcomes_total` | counter | `configuration`, `kind`, `outcome` | agent | Rule outcomes by rule kind and `changed`, `pending`, `unchanged`, `skipped`, or `failed` |
| `node_configuration_agent_last_run_timestamp_seconds` | gauge | `configuration` | agent | Unix time of the last run |
| `node_configuration_agent_last_run_failed_rules` | gauge | `configuration` | agent | Rules that failed in the last run |
| `node_configuration_controller_leader` | gauge | none | controller | 1 while this replica acts. Always 1 without leader election |

## Reading them

- `outcome="changed"` after the first run of a node means the node drifted and was corrected. A steady rate points at something else rewriting the same file.
- `outcome="pending"` is a difference a dry run found and left in place.
- `outcome="skipped"` means the target does not exist on that node, such as a missing account or file.

Rollout state is in the status of the resources, not in a metric. Watch the `Progressing` condition for `ProgressDeadlineExceeded`.

## Alert ideas

| Condition | Expression |
| --- | --- |
| No controller replica leads | `sum(node_configuration_controller_leader) < 1` |
| A node has failing rules | `node_configuration_agent_last_run_failed_rules > 0` |
| An agent stopped running | `time() - node_configuration_agent_last_run_timestamp_seconds > 3600` |
| Repeated drift on a node | `increase(node_configuration_agent_rule_outcomes_total{outcome="changed"}[1h]) > 5` |
