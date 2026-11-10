# Leader election

| Status | Category |
| --- | --- |
| implemented in 0.4.0 | availability |

## Summary

Run two replicas. A Kubernetes Lease picks one leader that polls AWS Health and posts to the Slack webhook. A ConfigMap ledger, written only through compare-and-swap, records every send so a failover, restart, or split-brain never posts the same event twice.

## Goals

- Zero duplicates caused by the notifier itself: failover, restart, split-brain.
- Failover within about `leaseDuration`. A new leader polls as soon as it is elected.
- No event dropped on restart or failover, unlike the former `cold_start_suppress` on every start.

## Non-goals

- Exactly-once delivery. The webhook has no idempotency key and no read-back.
- Bot token delivery (`chat.postMessage`).

## Components

| Resource | Role |
| --- | --- |
| Lease `<fullname>-leader` | Liveness. Decides who polls. |
| ConfigMap `<fullname>-ledger` | Safety. Send state, reminder state, poll cursor, fencing term. |

The Lease alone cannot prevent two leaders (GC pause, network partition). The ledger CAS does.

## Ledger

```json
{
  "term": 12,
  "holder": "ahen-7c9f-abcde",
  "cursor": "2026-11-10T03:00:00Z",
  "events": { "<arn>": { "lastUpdated": "2026-11-10T02:55:12Z", "state": "sent" } },
  "reminders": { "<arn>": { "24": "pending" } }
}
```

- Every write is a CAS on `resourceVersion`. A conflict means the ledger was claimed: stop the cycle.
- `state` is `pending`, `sent`, `skipped`, or `unknown`.
- Event entries older than 72h are pruned. Reminder entries go once the event leaves the upcoming window.
- The app creates the ConfigMap. A chart-owned one would be overwritten by Helm or Argo CD.
- An unreadable payload is replaced by an empty ledger, which cold-starts.

## Fencing

On acquiring the Lease, the new leader first writes `term + 1` and `holder` to the ledger. This changes `resourceVersion`, so every later write from the old leader fails, and it cannot record, so it cannot post. A replica that hits a conflict reloads the ledger. If the holder and term are still its own (an out-of-band edit), it continues. Otherwise it idles until it is elected again.

## Send flow

1. CAS the entry to `pending`.
2. POST to the webhook, retrying per [Delivery retry policy](#delivery-retry-policy).
3. CAS the result: `sent`, rolled back, `skipped`, or `unknown`.

Reminders use the same flow keyed by `(arn, offset)`.

## Delivery retry policy

The duplicates are split by cause. The notifier's own causes are held at zero by the ledger. Slack's own ambiguity follows the common practice of alerting systems (Alertmanager, Argo CD Notifications): a missed alert is worse than a rare duplicate, so an ambiguous outcome is retried a bounded number of times.

| Slack result | Posted? | Handling |
| --- | --- | --- |
| 200 | yes | `sent` |
| Connect error or connect timeout | no | Rolled back, retried next cycle |
| 429 | no | Rolled back, remaining sends deferred to the next cycle |
| 4xx other than below (`no_service`, `channel_is_archived`, ...) | no | Rolled back, retried next cycle |
| 400 `invalid_payload` | no | `skipped` and logged, since a retry fails again |
| 5xx, timeout after send | unknown | Retried in the cycle up to 3 attempts (1s, 2s backoff), then `unknown` |

- Not posted is always safe to retry. Rolling back keeps the cursor at that event's `lastUpdatedTime`, so it is listed again next cycle.
- A duplicate can occur only when Slack posted the message and still answered 5xx or timed out.
- `unknown` is never retried by anyone, and is alerted on (see [Observability](#observability)).
- The Slack client sets a 3s `connect_timeout` apart from the request timeout, so a request that never left the pod is distinguishable.

| Situation | Duplicates | Loss |
| --- | --- | --- |
| Leader change, restart | 0 | At most the send in flight at that moment, reported `unknown` |
| Brief Slack outage | Rare | 0 when recovered within 3 attempts |
| Long Slack outage | 0 | Reported `unknown` |

## Failover

- The leader writes `cursor` at the end of each cycle, held back to the oldest event left for retry.
- A new leader polls from `cursor - 5m`, never further back than 24h. The overlap absorbs late `lastUpdatedTime` updates, and the ledger drops the repeats.
- `pending` entries left by the old leader become `unknown`.
- `cold_start_suppress` applies only while the ledger has no cursor.
- On SIGTERM the poller finishes its cycle, then the leader releases the Lease.

## Startup

With leader election enabled, the pod fails at startup instead of running unfenced when `POD_NAME` or `POD_NAMESPACE` is missing, no in-cluster client can be built, or a `SelfSubjectAccessReview` denies any required verb. The timing must satisfy `0 < retryPeriod < renewDeadline < leaseDuration`.

## Configuration

| Value | Default |
| --- | --- |
| `leaderElection.enabled` | `false` (in-memory ledger, single replica) |
| `leaderElection.leaseDurationSeconds` | `15` |
| `leaderElection.renewDeadlineSeconds` | `10` |
| `leaderElection.retryPeriodSeconds` | `2` |

When enabled, the chart adds a Role with `create` on `leases` and `configmaps`, and `get` and `update` scoped by `resourceNames` to the Lease and the ledger. Set `replicaCount: 2` and `topologySpreadConstraints` across zones. The existing PDB (`maxUnavailable: 1`) keeps one replica up. Standbys report ready.

## Observability

| Metric | Meaning |
| --- | --- |
| `aws_health_event_leader` | 1 on the leader, always 1 without election |
| `aws_health_event_leader_transitions_total` | Lease acquisitions |
| `aws_health_event_ledger_conflicts_total` | CAS conflicts |
| `aws_health_event_delivery_unknown_total` | Possibly lost sends. Alert on increase. |

Each `unknown` also emits a Warning Event `DeliveryUnknown` with the ARN, so an operator can check the channel.

## Testing

- Unit: Lease decisions, CAS conflict, stale term rejected, `pending` to `unknown` on takeover, cursor overlap and cap, Slack result classification, retry then `unknown`.
- Kubernetes API: an in-process fake API server with `resourceVersion` CAS backs the Lease, ledger, elector, and poller tests.
