# High availability

Two or more replicas elect one leader through a Lease. Only the leader polls and posts, and a ConfigMap ledger keeps a replica that lost the Lease from posting the same event twice. The design, guarantees, and retry policy are in [Leader election design](designs/leader-election.md).

## Enable

```yaml
replicaCount: 2
leaderElection:
  enabled: true
topologySpreadConstraints:
  - maxSkew: 1
    topologyKey: kubernetes.io/hostname
    whenUnsatisfiable: ScheduleAnyway
    labelSelector:
      matchLabels:
        app.kubernetes.io/name: aws-health-event-notifier
```

The chart adds a Role for the Lease and the ledger ConfigMap. The pod fails at startup if that RBAC or the Downward API identity is missing.

## Check roles

```bash
kubectl get lease <fullname>-leader -o jsonpath='{.spec.holderIdentity}'
```

| Log message | Pod |
|-------------|-----|
| `acquired leadership` with `previous_leader` and `reason` | New leader |
| `standing by` with `leader` | Standby, on first sighting and on every leader change |
| `claimed the delivery ledger` with `abandoned` | New leader. `abandoned` above 0 means sends in flight at failover, each reported as `DeliveryUnknown` |

`aws_health_event_leader` is 1 on the leader and 0 on standbys. See [Metrics](metrics.md).
