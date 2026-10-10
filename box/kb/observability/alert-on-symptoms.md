---
description: Page on user-facing symptoms, p99 latency and error ratio, and keep cause metrics like CPU and memory for diagnosis, because causes fire without impact and miss impact without cause.
tags: [observability, alerting, prometheus, slo, golden-signals]
resources: [PrometheusRule]
status: adopted
reviewed: 2026-10-10
---

# Alert on symptoms, diagnose with causes

## Rule

Paging alerts fire on symptoms users feel, from the [four golden signals](https://sre.google/sre-book/monitoring-distributed-systems/#xref_monitoring_golden-signals). Cause metrics go to dashboards and non-paging warnings.

- Latency: alert on p99 from [`histogram_quantile`](https://prometheus.io/docs/prometheus/latest/querying/functions/#histogram_quantile), never on the average. Averages hide the tail
- Errors: alert on the 5xx ratio of total requests, never on a raw error count. Track 4xx separately, a 4xx spike after a deploy usually means a broken API contract
- Traffic: alert on a sudden drop against the same weekday last week. A drop often means requests are not reaching the servers at all
- Saturation: page only when it predicts an outage, such as a disk filling within hours. Otherwise CPU, memory, and pool usage are causes
- Read CPU together with latency and, in containers, with CFS throttling, see [omit-cpu-limits.md](../kubernetes/omit-cpu-limits.md). Low CPU with high p99 points to waiting on I/O, locks, or pools
- Annotate deploys on dashboards. Most incidents follow a change, so the first question in triage is what changed

## Why

Symptoms and causes fail in opposite ways as paging signals:

- **Causes fire without impact**: CPU at 90% on a batch worker hurts no one, and paging on it trains people to ignore pages
- **Causes miss impact**: a slow dependency raises latency through pool exhaustion while CPU stays low. By [Little's Law](https://en.wikipedia.org/wiki/Little%27s_law), 1000 req/s at 50 ms keeps 50 requests in flight, and at 2 s it needs 2000, far past a default 200-thread pool, with no traffic change
- **The tail is common**: at 1000 req/s, the slowest 1% is 10 requests every second. A page that makes 40 backend calls hits at least one p99 call about a third of the time (1 - 0.99^40)
- **Queues grow non-linearly**: relative wait is about 4x at 80% utilization and 19x at 95%, so latency degrades sharply before saturation alerts look alarming

Symptoms tell you that users are hurting and how badly. Causes then narrow down where, so they belong on the dashboard the page links to.

## Exceptions

- Rule catalogs such as [Awesome Prometheus Alerts](https://samber.github.io/awesome-prometheus-alerts/) are mostly cause-based. Use them as warning-level rules routed to a ticket queue through [Alertmanager routes](https://prometheus.io/docs/alerting/latest/configuration/#route), not as pages
- Components with no request path, such as batch jobs and controllers, page on their own outcome: job failure, reconcile errors, or staleness of the last successful run

## Example

```yaml
- alert: HighP99Latency
  expr: |
    histogram_quantile(0.99, sum by (le, service) (rate(http_server_request_duration_seconds_bucket[5m]))) > 0.5
  for: 10m
  labels:
    severity: critical
- alert: HighErrorRatio
  expr: |
    sum by (service) (rate(http_server_request_duration_seconds_count{http_response_status_code=~"5.."}[5m]))
    /
    sum by (service) (rate(http_server_request_duration_seconds_count[5m])) > 0.02
  for: 10m
  labels:
    severity: critical
```

## References

- [Monitoring Distributed Systems](https://sre.google/sre-book/monitoring-distributed-systems/)
- [서버 모니터링 분석 가이드](https://kciter.so/posts/server-monitoring-analysis-guide/)
