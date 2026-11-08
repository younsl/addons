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

### Through a Gateway

`spec.manager.httpRoute` makes the operator own a Gateway API `HTTPRoute` named `<name>-manager` that sends `/v1` on the listed host names to the admin API:

```yaml
spec:
  manager:
    httpRoute:
      enabled: true
      parentRefs:
        - group: gateway.networking.k8s.io
          kind: Gateway
          name: internal
          namespace: gateway-system
          sectionName: https
      hostnames:
        - ward.example.com
```

- Every `parentRefs` entry needs `namespace`, and that Gateway must allow routes from the `Ward` namespace.
- Only the admin API is routed. The agent API on port 8443 is mutual TLS and is never exposed.
- The admin API is plain HTTP behind a bearer token, so attach the route to an HTTPS listener and keep the Gateway internal.
- The `Ward` status reports `HTTPRouteReady`, `False` with the API server's error when the Gateway API CRDs are missing.

## Detection storage

Detections stay in `detections.jsonl` on the manager volume unless `spec.postgresql.enabled` is set. With PostgreSQL the manager still answers queries from memory, and the database is the durable copy, so detections survive the manager volume and other tools (SQL, Grafana) can read the `ward_detections` table. The same `retention` and per node share bound both.

| Setting | Server | Connection |
| --- | --- | --- |
| `postgresql.enabled: true` | Embedded: the operator deploys `<name>-postgresql`, a one replica StatefulSet with an 8Gi volume by default | TLS with a certificate the Ward CA issues, verified by host name, and a generated password in `<name>-postgresql-auth`. A NetworkPolicy admits only the manager, and `pg_hba.conf` refuses connections without TLS |
| `postgresql.external` set | Yours | `sslMode: VerifyFull` (default) checks the server certificate against `caSecretRef` and authenticates with SCRAM channel binding, so the server can never ask for the password in clear. The password and CA `Secrets` need the label `ward.sh/postgresql: "true"`. `Disable` sends everything in clear, needs `operator.allowPostgresqlPlaintext`, and works only because the operator then sets `WARD_POSTGRESQL_ALLOW_PLAINTEXT`: a manager without a CA bundle and without that setting refuses to connect, and logs a warning on every plaintext connect |

- On its first start against an empty table, the manager imports the detections already in `detections.jsonl`.
- Disabling the embedded server deletes it with its volume and password, so the next enable starts empty.
- The embedded server is a single replica without backups. Use an external, managed server when detections must survive the cluster.
- The embedded image comes from the operator setting `operator.postgresqlImage`, and a `Ward` may override it only within `operator.allowedImagePrefixes`.

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
