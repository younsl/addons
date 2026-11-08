# Metrics

The Prometheus metrics the operator, agent, and manager expose, and how to scrape them. Read this to build dashboards or alerts.

## Scraping

| Component | Endpoint | Scrape with |
| --- | --- | --- |
| Operator | Service `<release>`, port `http` (8081), `/metrics` | `serviceMonitor.enabled=true` |
| Agent | Pod port `metrics` (8081), `/metrics` | `podMonitor.enabled=true` |
| Manager | Pod port `metrics` (8081), `/metrics` | `podMonitor.enabled=true` |

The PodMonitor selects every Pod labeled `app.kubernetes.io/name=ward` with component `manager` or `agent` and adds `component` and `node` labels.

## Operator

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `ward_operator_reconcile_total` | counter | `result` | Reconciles by outcome |
| `ward_operator_leader` | gauge | | 1 on the replica holding the leader Lease |
| `ward_feed_pull_total` | counter | `result` | Feed checks: `updated`, `unchanged`, `verify_failed`, `error` |
| `ward_rollout_phase` | gauge | `namespace`, `ward`, `phase` | 1 for the current phase: `Idle`, `Canary`, `Halted` |
| `ward_agents` | gauge | `namespace`, `ward`, `source` | Agents per event source mode |

## Agent

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `ward_agent_detections_total` | counter | `engine`, `rule`, `severity`, `action` | Detections and the action taken |
| `ward_agent_events_total` | counter | `kind` | Kernel or polled events: `exec`, `exit`, `connect`, `block` |
| `ward_agent_events_dropped_total` | counter | | Ring buffer records lost because the agent fell behind |
| `ward_agent_files_scanned_total` | counter | `trigger` | Files scanned on `exec` or by the `full` scan |
| `ward_agent_full_scan_duration_seconds` | histogram | | Duration of each full scan |
| `ward_agent_content_info` | gauge | `revision`, `source` | 1 for the active database revision and event source mode |
| `ward_agent_content_swap_total` | counter | `result` | Revision swaps: `ok` or `rejected` |
| `ward_agent_blocklist_entries` | gauge | | Inodes in the BPF LSM blocklist |
| `ward_agent_whodata_inodes` | gauge | | Files and directories the integrity whodata hooks watch |

## Manager

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `ward_manager_detections_received_total` | counter | | Detections posted by agents |
| `ward_manager_detections_stored` | gauge | | Detections in the store |
| `ward_manager_content_served_total` | counter | `revision` | Database downloads served to agents |
| `ward_manager_http_requests_total` | counter | `endpoint`, `code` | API requests by status code |

## Alert examples

```promql
# Any critical detection
increase(ward_agent_detections_total{severity="Critical"}[5m]) > 0

# A feed revision was halted
ward_rollout_phase{phase="Halted"} == 1

# An agent is losing kernel events
rate(ward_agent_events_dropped_total[5m]) > 0

# A node fell back to /proc polling
ward_agents{source="procfs"} > 0
```

## Conclusion

Alert on critical detections, halted rollouts, dropped events, and agents that lost eBPF. The detections themselves are in logs, Events, and the manager store.
