# Metrics

Prometheus metrics tether serves at /metrics, what moves them, and queries to watch them with.

## Endpoint

GET /metrics returns the OpenMetrics text format (application/openmetrics-text). It sits outside the loopback guard like /healthz and /readyz, so any Host header reaches it. Every name carries the tether_ prefix. Counters reset when the process restarts.

```bash
curl -s 127.0.0.1:8080/metrics
```

## Reference

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| tether_reconcile_runs_total | counter | | Reconcile runs started, failed ones included |
| tether_reconcile_failures_total | counter | error | Reconcile runs that failed, by cause |
| tether_reconcile_duration_seconds | histogram | | Time one reconcile took, buckets from 1ms to about 8s |
| tether_link_entries | gauge | action | Link entries in the last reconcile by planned action |
| tether_link_failed_entries | gauge | | Link entries that failed to apply in the last reconcile |
| tether_backups_total | counter | | Files and directories moved to the backup directory |
| tether_package_file_updates_total | counter | | Brewfile and krewfile rewrites because their package list changed |
| tether_last_success_timestamp_seconds | gauge | | Unix time the last successful reconcile started, 0 until the first one |
| tether_build_info | gauge | version, commit, rustc | Always 1, carries build metadata |

### error label

| Value | Cause |
| --- | --- |
| config_read | config.toml could not be read |
| config_parse | config.toml is not valid TOML or has unknown keys |
| config_invalid | A path is not absolute, or two links share a target |
| link | At least one link entry failed to apply, the run itself finished |
| package_file | At least one package file could not be regenerated |

The first three abort the run before any link is touched. link and package_file count once per run, however many entries failed.

### action label

Every value is always present, at 0 when no entry has it: in_sync, create, relink, backup, make_dir, missing_source. The meaning of each is in [Architecture](architecture.md#reconcile-loop).

## Behavior

- tether_link_entries describes the last run only. A run that creates a link reports create, and the next run reports the same entry as in_sync. A run aborted by a config error sets every action to 0.
- In dry run, tether_link_entries shows what would change, and tether_backups_total and tether_package_file_updates_total stay at 0 because nothing is written.
- tether_last_success_timestamp_seconds advances only when the config loads and every link entry applies. A package file error does not hold it back, it only counts under error="package_file".
- A run that panics still counts in tether_reconcile_runs_total and the duration histogram, but in no failure series.

## Queries

| Purpose | PromQL |
| --- | --- |
| Reconciles failing | increase(tether_reconcile_failures_total[15m]) > 0 |
| No success for three default intervals | time() - tether_last_success_timestamp_seconds > 900 |
| A configured source is missing | tether_link_entries{action="missing_source"} > 0 |
| Links failing to apply | tether_link_failed_entries > 0 |
| Real files replaced by links | increase(tether_backups_total[1d]) > 0 |
| p95 reconcile duration | histogram_quantile(0.95, sum by (le) (rate(tether_reconcile_duration_seconds_bucket[1h]))) |
| Running version | tether_build_info |

Scale the staleness threshold with RECONCILE_INTERVAL when it differs from the 5m default.

## Scrape

tether listens on loopback, so Prometheus has to run on the same host.

```yaml
scrape_configs:
  - job_name: tether
    static_configs:
      - targets: ["127.0.0.1:8080"]
```

## Conclusion

Two series cover most of what can go wrong: tether_reconcile_failures_total says a run broke and why, and tether_last_success_timestamp_seconds says links have stopped being kept in place. Alert on those first. tether_link_entries and tether_backups_total explain what a run changed, and the duration histogram and tether_build_info help when a run slows down or after an upgrade.
