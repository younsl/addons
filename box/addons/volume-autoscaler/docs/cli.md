# Built-in CLI

## Overview

The `volume-autoscaler` binary ships operational subcommands alongside
the controller, in the style of Grafana Alloy. They let you validate the config
file and inspect which instances each [InstanceVolumeAutoscaler](instance-volume-autoscaler.md) reaches without a running controller: from a laptop
before deploying, or inside the [Pod][k8s-pod] with `kubectl exec` after.

Read this if you are:

- A platform or DevOps engineer writing or changing autoscalers who wants
  to confirm what they target before they take effect.
- An on-call engineer checking why an instance was (or was not) resized.

```
Usage: volume-autoscaler [OPTIONS] [COMMAND]

Commands:
  run          Run the controller (the default when no subcommand is given)
  validate     Load and validate the config file, then exit
  autoscalers  Print the InstanceVolumeAutoscalers and their effective settings (reads the Kubernetes API)
  instances    List discovered instances with the autoscaler that targets each (calls AWS and the Kubernetes API)
  crd          Print a CustomResourceDefinition as JSON
  unused       List unused PersistentVolumeClaims and PersistentVolumes (reads the Kubernetes API, writes nothing)
  help         Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  Path to the config file (else the mounted default) [env: CONFIG_FILE=]
  -v, --verbose          Verbose output (debug logging) [env: VERBOSE=]
      --controllers <CONTROLLERS>
                         Controllers to run, comma separated [env: CONTROLLERS=]
                         [default: instance-volume-autoscaler,persistent-volume-claim-autoscaler,unused-volume-scan]
  -h, --help             Print help
  -V, --version          Print version
```

With no subcommand (or `run`) the binary starts the controller. All commands
exit `0` on success and non-zero on any error, so they compose with CI checks
and shell scripts.

## The --config flag

Every command reads the same config file, resolved in this order:

1. `--config <path>` flag
2. `CONFIG_FILE` environment variable
3. `/etc/volume-autoscaler/config.yaml` (the chart's [ConfigMap][k8s-configmap] mount)

## The --controllers flag

One image runs every controller. `--controllers` (env `CONTROLLERS`, chart value `controllers`) picks which ones this process runs, comma separated:

- `instance-volume-autoscaler`: grows EC2 root volumes. The throughput recommender runs with it when `throughputRecommendation.enabled` is set.
- `persistent-volume-claim-autoscaler`: grows PersistentVolumeClaims. Requires `metrics.url`.
- `unused-volume-scan`: reports unused claims and volumes.

All three run by default. An unknown name fails at startup. The chart grants RBAC only for the enabled controllers.

```bash
volume-autoscaler --controllers persistent-volume-claim-autoscaler
```

## Commands

### validate

Loads the config file, applies defaults, strict-decodes it (unknown keys are errors), and validates every field. Exits non-zero with the first error. Never contacts AWS or the Kubernetes API.

```console
$ volume-autoscaler validate --config config.example.yaml
config config.example.yaml is valid: region=ap-northeast-2
```

Typical use: a CI check on config changes. A file that still carries the retired `defaultPolicy` or `policies` key fails here with a pointer to InstanceVolumeAutoscaler.

### autoscalers

Lists the InstanceVolumeAutoscalers in the cluster (through the kubeconfig, or in-cluster credentials inside the Pod) with their **effective** settings, defaults filled in, in name order. Rejected autoscalers follow with the reason. Never contacts AWS unless `--count` is set.

```console
$ volume-autoscaler autoscalers --config config.example.yaml
NAME      TARGET                              UTILIZATION%  SCALE_UP        MAX_GIB  COOLDOWN  ALERT
bastion   Instance/i-0123456789abcdef0        80            max(10%,1GiB)   100      6h        true
database  TagSelector/Env=prod,Role=database  70            max(20%,50GiB)  2048     6h        true
broken    (invalid: scaleTargetRef.tags entries need a non-empty key and value)
```

- `TARGET` is the `scaleTargetRef`, one instance ID or the required tags.
- `UTILIZATION%` is the `storage` target that triggers a scale up.
- `SCALE_UP` is the step rule: `max(...)` or `min(...)` over the policies, or `disabled`.
- `MAX_GIB` is `maxCapacity` in whole GiB.
- `COOLDOWN` is the effective `cooldownDuration`, `6h` by default.
- `ALERT` is the effective `alertEnabled`.

With `--count`, it also discovers instances via AWS and appends a `MATCHED` column with the number of instances each autoscaler reconciles (equals the `volume_autoscaler_policy_instances` metric). An autoscaler held back as ambiguous counts 0.

### instances

Discovers instances exactly as the controller does and lists each with the autoscaler that targets it. Requires AWS credentials with `ec2:DescribeInstances` and `ec2:DescribeVolumes` and read access to InstanceVolumeAutoscalers. Makes no writes. An instance no autoscaler targets shows `(none)`, and one claimed by overlapping autoscalers shows `(ambiguous: a,b)`.

```console
$ volume-autoscaler instances --config config.example.yaml
AUTOSCALER  INSTANCE_ID          NAME           ROOT_VOLUME            SIZE_GIB
bastion     i-0123456789abcdef0  bastion-01     vol-0123456789abcdef0  30
database    i-0a1b2c3d4e5f67890  prod-db-01     vol-0a1b2c3d4e5f67890  500
(none)      i-0fedcba9876543210  shared-web-01  vol-0fedcba9876543210  120

3 instances discovered in ap-northeast-2, 2 targeted
```

Use it to answer "which autoscaler governs this instance?" and to confirm a `scaleTargetRef` change before applying it.

### crd

Prints the CustomResourceDefinition the controller derives from its Rust types. `--kind` picks `instance-volume-autoscaler` (the default) or `persistent-volume-claim-autoscaler`. `make crd` renders both into the chart, so the chart schema never drifts from the code.

### unused

Lists the [PersistentVolumeClaims][k8s-pvc] and [PersistentVolumes][k8s-pv] no workload is using,
sorted longest-unused first. It reads the Kubernetes API and writes nothing, so
it is safe to run at any time. Requires in-cluster access, so it runs inside the
Pod rather than from a laptop.

```console
$ kubectl exec deploy/volume-autoscaler -- volume-autoscaler unused
KIND                   NAMESPACE  NAME               REASON                   UNUSED_FOR  CAPACITY  STORAGE_CLASS  EBS_VOLUME             BOUND_TO
persistentvolume       -          pvc-9f2c1a4b       released                 512h0m0s    100.0Gi   gp3            vol-0a1b2c3d4e5f67890  legacy/reports
persistentvolumeclaim  analytics  data-clickhouse-3  statefulset_scaled_down  336h0m0s    500.0Gi   gp3            vol-0123456789abcdef0  pvc-3d7e2b91
persistentvolumeclaim  legacy     uploads            no_consumer_pod          72h0m0s     20.0Gi    gp3            vol-0fedcba9876543210  pvc-77c1e004

3 unused objects holding 620.0Gi, out of 214 objects scanned (minUnusedAge 24h0m0s)
```

| Column | Meaning |
|--------|---------|
| `KIND` | `persistentvolumeclaim` or `persistentvolume` |
| `NAMESPACE` | The claim's namespace, `-` for a cluster-scoped volume |
| `REASON` | Why it is unused. See [Unused volume identification](unused-volumes.md#what-counts-as-unused) for each value |
| `UNUSED_FOR` | How long it has been continuously unused |
| `CAPACITY` | Provisioned size, the number that turns the report into a cost |
| `EBS_VOLUME` | The EBS volume ID to price or delete in EC2, `-` when not EBS-backed |
| `BOUND_TO` | The bound volume for a claim, the bound claim for a volume |

The `UNUSED_FOR` clock lives in the [annotation][k8s-annotations] the scan loop writes, so a freshly
started controller reports every object as unused since now until its first pass
lands. Which objects are listed is unaffected. Pass `--all` to include objects
that have not yet been unused for 24 hours.

Nothing in this command deletes anything, and neither does the loop behind it.
"Unused" is an observation about the cluster's current state, not a statement
that the data is disposable: a claim held for a quarterly job and a claim nobody
will ever read again look identical from here.

### run

Starts the controllers `--controllers` enables (identical to running with no subcommand): loads the config, serves health and metrics, elects a leader when enabled, and reconciles on the configured intervals.

## Running inside the Pod

The container image ships the same binary, so every command works via
`kubectl exec` against the mounted ConfigMap:

```bash
kubectl exec deploy/volume-autoscaler -- volume-autoscaler autoscalers --count
kubectl exec deploy/volume-autoscaler -- volume-autoscaler instances
kubectl exec deploy/volume-autoscaler -- volume-autoscaler unused
```

The Pod's IRSA/Pod Identity credentials cover the read-only EC2 calls, since
the controller already requires them. `unused` needs no AWS credentials at all,
only the [ClusterRole][k8s-rbac] the chart creates whenever `rbac.create` is true.

## Local verification

`config.example.yaml` at the project root mirrors the chart-rendered config.
The Makefile wraps each command against it; override the file with
`CONFIG=path`:

```bash
make validate     # validate     --config config.example.yaml
make autoscalers  # autoscalers  --config config.example.yaml (needs a kubeconfig)
make instances    # instances    --config config.example.yaml (needs AWS credentials and a kubeconfig)
make crd          # crd, rendered into the chart
```

`unused` has no local equivalent: it reads the Kubernetes API through the
in-cluster config, which does not exist outside a Pod.

## Conclusion

Three read-only questions, three commands:

- Is the config valid? `validate`
- What does each autoscaler do, and how many instances does it cover? `autoscalers [--count]`
- Which autoscaler governs which instance? `instances`

Wire `validate` into CI for config changes, and reach for `kubectl describe iva` and `autoscalers --count` first when a resize did not happen where you expected one.

[k8s-pod]: https://kubernetes.io/docs/concepts/workloads/pods/
[k8s-configmap]: https://kubernetes.io/docs/concepts/configuration/configmap/
[k8s-pvc]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/#persistentvolumeclaims
[k8s-pv]: https://kubernetes.io/docs/concepts/storage/persistent-volumes/
[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-rbac]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#role-and-clusterrole
