# Manager

What turning the manager on adds, how agents talk to it, and what turning it off changes. The manager is on by default (`spec.manager.enabled: true`).

## What it adds

- **Feed relay**: The manager caches signature database revisions on its PersistentVolumeClaim and serves them to agents. Only the manager needs registry egress.
- **Detection store**: Agents post every detection to the manager. It keeps the newest `spec.manager.retention` detections across restarts, and no node holds more than a tenth of it (at least 100), so one noisy or compromised node cannot push the others out.
- **Query API**: Read detections and agent state on port 8080 with a bearer token.
- **Mutual TLS and node identity**: Agents and manager authenticate each other on port 8443, and every request also carries the agent's node-bound token.

Detections still go to stdout JSON and Kubernetes Events on every node, the manager is an addition, not a replacement.

## Workload

| Object | Name |
| --- | --- |
| StatefulSet | `<name>-manager`, one replica |
| Headless Service | `<name>-manager`, ports 8443 (agent mTLS), 8080 (query API), 8081 (`metrics`) |
| PersistentVolumeClaim | from `spec.manager.storage`, immutable after creation |
| API token Secret | `<name>-api-token` |
| PKI Secrets | `<name>-ca`, `<name>-manager-tls`, `<name>-agent-tls` |

## Query API

Read the token and port-forward:

```bash
TOKEN=$(kubectl -n ward-system get secret ward-api-token -o jsonpath='{.data.token}' | base64 -d)
kubectl -n ward-system port-forward svc/ward-manager 8080:8080
curl -s -H "Authorization: Bearer ${TOKEN}" 'localhost:8080/v1/detections?severity=Critical&limit=20'
```

`/v1/detections` filters by `severity`, `namespace`, `node`, and `engine` and returns newest first. `/v1/agents` lists agents with their last report.

## mTLS and PKI

The operator owns a private CA in `<name>-ca` and issues the manager's server certificate (`<name>-manager-tls`, valid for the Service DNS names) and the agents' client certificate (`<name>-agent-tls`). The manager accepts only clients the CA signed, and agents accept only a server the CA signed.

The client certificate is shared by every agent, so it proves membership, not which node is calling. Each agent therefore also sends a projected ServiceAccount token with the audience `ward-manager`, rotated hourly by the kubelet. The manager checks it with a TokenReview and accepts only the `ward-agent` ServiceAccount, taking the node from the token's bound node claim. The `node` field of a posted detection is replaced by that node, so a compromised node can report only for itself. Verified tokens are cached for five minutes.

The operator renews a certificate well before it expires and the manager and agents reload the mounted Secrets without a restart. Deleting `<name>-ca` rotates the whole chain on the next reconcile.

## Turning it off

Set `spec.manager.enabled: false` and the operator deletes the StatefulSet, its Service, and the PKI and token Secrets. Then:

- each agent pulls its assigned revision from the registry, with `spec.feed.pullSecretRef` mounted, so every node needs registry egress
- detections leave the node only as stdout JSON, Events, and Lease counters, so collect them with your log pipeline
- the rollout, verification, and halts work the same, because the operator owns them

The PersistentVolumeClaim is kept so stored detections survive. Delete it by hand when you no longer need them:

```bash
kubectl -n ward-system delete pvc -l app.kubernetes.io/name=ward,app.kubernetes.io/component=manager
```

## Conclusion

Run the manager when nodes should not reach the registry or when you want a detection store with an API. Turn it off for the smallest footprint, where logs and Events are enough.
