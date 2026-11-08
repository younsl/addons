# Configuration

Reference for the `Ward` spec and the `WardSignature` resource. Every field has a default, so `spec: {}` is a valid `Ward`. The API server enforces the rules listed in [Validation](validation.md).

## Ward

A `Ward` is namespaced. Its name is at most 40 characters so derived object names stay valid. Only one `Ward` per cluster is supported, a second one is reported in its status.

### Who may write a Ward

A `Ward` runs an agent with `hostPID` and broad capabilities on every node, so create or update on `wards` is as powerful as cluster-admin. Grant it only to cluster administrators, and never add it to the aggregated `admin` or `edit` roles. The chart does not aggregate it. The operator also holds every `Ward` to three rules and reports a refused one as `Degraded` without deploying anything:

| Rule | Reason | Operator setting |
| --- | --- | --- |
| The `Ward` lives in the operator's namespace (the chart's release namespace) | `NamespaceNotAllowed` | Fixed. The chart's `wardNamespacePolicy` also rejects it at admission |
| `spec.image` starts with an allowed prefix | `ImageNotAllowed` | `operator.allowedImagePrefixes`, empty allows only the chart's own image repository |
| `spec.agent.privileged` is only honored when allowed | `PrivilegedNotAllowed` | `operator.allowPrivilegedAgent`, `false` by default |
| `spec.postgresql.embedded.image` stays within the allowed prefixes, or equals the operator's PostgreSQL image | `ImageNotAllowed` | `operator.postgresqlImage`, `operator.allowedImagePrefixes` |
| `spec.postgresql.external.sslMode: Disable` is only honored when allowed | `PlaintextNotAllowed` | `operator.allowPostgresqlPlaintext`, `false` by default |
| `passwordSecretRef` and `caSecretRef` name a `Secret` labeled `ward.sh/postgresql: "true"`, never one the operator generates | `SecretNotAllowed` | Fixed, so a `Ward` cannot send another `Secret` to a server it names |

### Top level

| Field | Default | Description |
| --- | --- | --- |
| `image` | operator image | Image of the manager and agent |
| `imagePullPolicy` | chart value | `Always`, `IfNotPresent`, or `Never` |
| `imagePullSecrets` | none | Secrets the manager and agent Pods pull their image with |
| `mode` | `Detect` | `Detect` reports only. `Protect` also kills, quarantines, and blocks exec. See [Response](response.md) |

### manager

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Run the manager. See [Manager](manager.md) |
| `storage.size` | `1Gi` | PVC size, at least `64Mi`. Immutable |
| `storage.storageClassName` | cluster default | PVC storage class. Immutable |
| `retention` | `10000` | Detections the manager keeps, 100 to 1000000 |
| `emitEvents` | `true` | Publish a Kubernetes Event on the affected Pod |
| `resources`, `nodeSelector`, `tolerations`, `affinity`, `topologySpreadConstraints` | none | Manager Pod resources and scheduling, standard Kubernetes fields |
| `dnsPolicy`, `dnsConfig` | Kubernetes default | Manager Pod DNS. `dnsPolicy: None` needs at least one `dnsConfig.nameservers` entry, which CEL enforces |
| `httpRoute.enabled` | `false` | Own a Gateway API `HTTPRoute` to the admin API. See [Manager](manager.md#through-a-gateway) |
| `httpRoute.parentRefs` | none | `group` (default `gateway.networking.k8s.io`), `kind` (default `Gateway`), `name`, `namespace` (required), `sectionName`, `port`. At least one when enabled, which CEL enforces |
| `httpRoute.hostnames`, `httpRoute.labels`, `httpRoute.annotations` | none | Route host names and metadata |

### postgresql

Where the manager keeps detections. See [Manager](manager.md#detection-storage).

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Keep detections in PostgreSQL. Needs `manager.enabled`, which CEL enforces |
| `embedded.image` | operator setting | Image of the embedded server |
| `embedded.storage.size`, `embedded.storage.storageClassName` | `8Gi`, cluster default | Data volume. Immutable |
| `embedded.resources`, `embedded.nodeSelector`, `embedded.tolerations`, `embedded.affinity` | none | Embedded server Pod resources and scheduling |
| `external.host`, `external.port` | required, `5432` | An existing server instead of the embedded one. `external` and `embedded` are exclusive |
| `external.database`, `external.username` | `ward` | Database and role. The role needs `CREATE` on the database for the first start |
| `external.passwordSecretRef` | required | `name` and `key` of the password in the `Ward` namespace. The `Secret` needs the label `ward.sh/postgresql: "true"` |
| `external.sslMode` | `VerifyFull` | `VerifyFull` or `Disable`. `VerifyFull` also requires SCRAM with channel binding, so the role's password must be stored as SCRAM (the default since PostgreSQL 14). `Disable` needs `operator.allowPostgresqlPlaintext` |
| `external.caSecretRef` | none | `name` and `key` of the PEM CA bundle, in a `Secret` with the same label. Required with `VerifyFull` |

### agent

| Field | Default | Description |
| --- | --- | --- |
| `eventSource` | `Auto` | `Auto`, `Ebpf`, or `Procfs`. See [eBPF probes](ebpf.md#event-source-modes) |
| `privileged` | `false` | Run privileged where AppArmor or SELinux blocks `/proc` access or eBPF. Refused unless the operator runs with `operator.allowPrivilegedAgent` |
| `dnsPolicy`, `dnsConfig` | Kubernetes default | Agent Pod DNS. Agents resolve the API server and, with the manager on, `<name>-manager.<namespace>.svc`, so keep cluster DNS reachable. `dnsPolicy: None` needs at least one `dnsConfig.nameservers` entry |
| `tolerations` | tolerate all | Unset tolerates every taint so every node is covered |
| `resources`, `nodeSelector`, `affinity`, `priorityClassName` | none | Agent Pod resources and scheduling, standard Kubernetes fields |
| `updateStrategy.type` | `RollingUpdate` | `RollingUpdate` or `OnDelete` |
| `updateStrategy.rollingUpdate` | `maxUnavailable: 10%` | `maxUnavailable` and `maxSurge`, a count or a percentage. Only with `RollingUpdate`, which CEL enforces. `maxUnavailable: 0` with `maxSurge: 1` starts the new agent before the old one stops, so no node goes unprotected during a rollout |

### scan

| Field | Default | Description |
| --- | --- | --- |
| `onExec` | `true` | Scan the executable of every new process |
| `yara` | `true` | Match YARA rules from the feed and `WardSignatures` |
| `interval` | `6h` | Full scan period of container filesystems and `hostPaths`, at least `1m` |
| `maxFileSizeMib` | `32` | Larger files are skipped, 1 to 1024 |
| `hostPaths` | none | Absolute host directories the full scan also walks, up to 64 |

### behavior

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Run the [behavior rules](detection-engines.md#behavior-rules) |
| `pollInterval` | `1s` | `/proc` sampling period in `procfs` mode, 100ms to 1m |
| `disabledRules` | none | Rule IDs to turn off, each at most once |

### integrity

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Run file integrity monitoring |
| `paths` | sensitive host files | Absolute host files and directories (one level deep), up to 256 |
| `interval` | `5m` | Check period, at least `10s` |

### rootcheck

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Run the [rootkit anomaly checks](detection-engines.md#rootcheck) |
| `interval` | `12h` | Check period, at least `10m`. A run probes every PID up to `pid_max` |

### exclusions

| Field | Description |
| --- | --- |
| `namespaces` | Namespaces whose Pods are never scanned or acted on |
| `paths` | Absolute path prefixes, in a container or on the host, never scanned |
| `processes` | Absolute executable paths, matched exactly against `/proc/<pid>/exe`, that behavior rules skip. Signature scanning and its response still apply. Names and basenames are never matched because any process can choose them |

### indicators

`allow` and `block` are lists of `{sha256, comment}`, each keyed by `sha256` and up to 10000 entries. An allow indicator clears a file whatever else matches. A block indicator is a detection named after its `comment`. A hash in both lists is reported in the `Ward` status. See [Detection engines](detection-engines.md#precedence).

### feed

| Field | Default | Description |
| --- | --- | --- |
| `image` | required | OCI reference by tag, or by `@sha256:` digest to pin a revision |
| `publicKey` | required | PEM ECDSA P-256 key the signature must verify against |
| `insecureSkipVerify` | `false` | Accept an unsigned database. For development only |
| `interval` | `6h` | Registry check period, at least `5m` |
| `pullSecretRef.name` | none | `kubernetes.io/dockerconfigjson` Secret for a private registry |
| `rollout.canaryPercent` | `10` | Share of nodes in the canary ring, besides `ward.sh/ring=canary` nodes |
| `rollout.soak` | `1h` | Canary run time before promotion, at least `1m` |
| `rollout.maxDetectionRatio` | `5` | Halt when the canary rate exceeds the broad rate by this factor |
| `rollout.minDetections` | `20` | Counts below this never halt a rollout |

See [Signature database](signature-database.md).

## WardSignature

A `WardSignature` adds signatures and YARA rules to the `Ward` in its namespace. The operator compiles every `WardSignature` into the policy and sets its `Accepted` condition and `status.signatures` count.

```yaml
apiVersion: security.ward.sh/v1alpha1
kind: WardSignature
metadata:
  name: custom
  namespace: ward-system
spec:
  signatures:
    - name: Custom.Miner.Stratum
      severity: High
      strings:
        - stratum+tcp://
        - --donate-level
      condition: All
  yara: |
    rule Custom_Reverse_Shell_Oneliner
    {
      strings:
        $tcp = "/dev/tcp/" ascii
      condition:
        $tcp
    }
```

| Field | Description |
| --- | --- |
| `signatures[].name` | Unique within the object, becomes the detection name |
| `signatures[].severity` | `Low`, `Medium`, `High`, or `Critical` |
| `signatures[].description` | Free text carried into the detection |
| `signatures[].sha256` | File hashes that match outright |
| `signatures[].strings` | Literal byte strings |
| `signatures[].hex` | Byte patterns in hex |
| `signatures[].condition` | `All` (default) or `Any` of the patterns |
| `yara` | YARA rule source compiled with YARA-X, up to 256 KiB |

A signature needs at least one of `sha256`, `strings`, or `hex`. YARA compile errors set `Accepted` to `False` with the compiler message, and the rest of the object still applies. The full example is [examples/signatures.yaml](../examples/signatures.yaml).

## Conclusion

The `Ward` spec holds the policy, and `WardSignature` objects add detection content as code. Both are checked at the API server first and in the operator second, see [Validation](validation.md).
