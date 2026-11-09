# Validation

ward validates its custom resources in the CRD schema and with [CEL rules](https://kubernetes.io/docs/reference/using-api/cel/) that the API server enforces, with no admission webhook. A rule CEL cannot express, because it spans several objects, is reported as a status condition instead of a rejection. The operator re-runs every check on objects stored before a CRD upgrade.

## Rejected by the API server

### Ward

| Field | Rule |
| --- | --- |
| `metadata.name` | At most 40 characters, so derived object names stay valid |
| `spec.manager.storage` | Immutable after creation, a StatefulSet cannot change its volume claims |
| `spec.manager.storage.size` | A quantity of at least `64Mi` |
| `spec.manager.retention` | 100 to 1000000 |
| `spec.scan.interval` | A duration of at least `1m` |
| `spec.scan.maxFileSizeMib` | 1 to 1024 |
| `spec.scan.hostPaths` | Absolute paths, up to 64 |
| `spec.behavior.pollInterval` | Between `100ms` and `1m` |
| `spec.behavior.disabledRules` | Known rule IDs only, each at most once |
| `spec.integrity.paths` | Absolute paths, up to 256 |
| `spec.integrity.interval` | A duration of at least `10s` |
| `spec.exclusions.paths` | Absolute paths |
| `spec.indicators.allow`, `spec.indicators.block` | Lowercase 64-digit SHA-256, unique within each list, up to 10000 |
| `spec.feed` | `keyless` or `publicKey` is required unless `insecureSkipVerify` is `true`, and not both |
| `spec.feed.image` | An OCI reference with a tag or an `@sha256:` digest |
| `spec.feed.interval` | A duration of at least `5m` |
| `spec.feed.rollout.canaryPercent` | 0 to 100 |
| `spec.feed.rollout.soak` | A duration of at least `1m` |
| `spec.feed.rollout.maxDetectionRatio` | 1 to 1000 |
| `spec.feed.rollout.minDetections` | 1 to 100000 |

Durations are written as `500ms`, `30s`, `5m`, `6h`, or combinations such as `1h30m`.

### WardSignature

| Field | Rule |
| --- | --- |
| `spec.signatures` | Up to 1000, `name` unique within the object |
| `spec.signatures[]` | Needs at least one of `sha256`, `strings`, or `hex` |
| `spec.signatures[].severity` | `Low`, `Medium`, `High`, or `Critical` |
| `spec.signatures[].condition` | `All` or `Any` |
| `spec.yara` | Up to 256 KiB |

A rejected object returns the rule's message:

```console
$ kubectl apply -f ward.yaml
The Ward "ward" is invalid: spec.feed: Invalid value: "object": feed needs keyless or publicKey unless insecureSkipVerify is true
```

## Reported as status conditions

These checks need more than one object or a compiler, so the API server cannot run them. The operator reports them and keeps running with the parts that are valid.

| Check | Where it shows |
| --- | --- |
| YARA source that does not compile | `WardSignature` condition `Accepted` set to `False` with the compiler message. Its other signatures still apply |
| A second `Ward` in the cluster | The newer `Ward` stays `Degraded` and deploys nothing |
| A hash in both `indicators.allow` and `indicators.block` | `Ward` condition with the overlapping hashes. Allow wins |

```bash
kubectl get wardsignatures -A
kubectl get ward ward -n ward-system -o jsonpath='{.status.conditions}' | jq
```

## Conclusion

Typos, bad durations, and unsafe values fail at `kubectl apply`, and cross-object problems surface in status without stopping detection.
