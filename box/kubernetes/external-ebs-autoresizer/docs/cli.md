# Built-in CLI

## Overview

The `external-ebs-autoresizer` binary ships operational subcommands alongside
the controller, in the style of Grafana Alloy. They let you validate the config
file and inspect policy reach without a running controller: from a laptop
before deploying, or inside the Pod with `kubectl exec` after.

Read this if you are:

- A platform or DevOps engineer writing or changing resize policies who wants
  to confirm what they match before they take effect.
- An on-call engineer checking why an instance was (or was not) resized.

```
Usage: external-ebs-autoresizer [OPTIONS] [COMMAND]

Commands:
  run        Run the controller (the default when no subcommand is given)
  validate   Load and validate the config file, then exit
  policies   Print the resolved resize policies and their effective settings
  instances  List discovered instances grouped by the policy each matches (calls AWS)
  unused     List unused PersistentVolumeClaims and PersistentVolumes (reads the Kubernetes API, writes nothing)
  help       Print this message or the help of the given subcommand(s)

Options:
      --config <CONFIG>  Path to the config file ($CONFIG_FILE, else the mounted default)
  -v, --verbose          Verbose output (debug logging)
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
3. `/etc/external-ebs-autoresizer/config.yaml` (the chart's ConfigMap mount)

## Commands

### validate

Loads the config file, applies defaults, strict-decodes it (unknown keys are
errors), validates every field, and compiles every resize policy (selector
regexes, grow amounts, required fields). Exits non-zero with the first error.
Never contacts AWS.

```console
$ external-ebs-autoresizer validate --config config.example.yaml
config config.example.yaml is valid: region=ap-northeast-2, 2 named resize policies plus the default
```

Typical uses: a CI check on config changes, and a pre-apply sanity check for a
new policy entry.

### policies

Prints each named policy and its **effective** settings, what an instance
matching it actually gets after inheriting unset fields from `defaultPolicy`,
sorted by precedence (highest weight first), with the default policy last.
Never contacts AWS unless `--count` is set.

```console
$ external-ebs-autoresizer policies --config config.example.yaml
POLICY   WEIGHT  SELECTOR                        PAUSED  THRESHOLD%  GROW             MAX_GIB
bastion  5       name~bastion                    true    60          percent +10%     1000
shared   1       name~^shared-                   false   80          absolute +50GiB  1000
default  -       (instances matching no policy)  false   80          percent +10%     1000
```

| Column | Meaning |
|--------|---------|
| `POLICY` | Policy name; `default` is the built-in bucket for unmatched instances |
| `WEIGHT` | Match precedence; highest wins when several policies match one instance |
| `SELECTOR` | Compact selector: `Key=Value` tag equalities and `name~<regex>`, ANDed with ` & ` |
| `PAUSED` | `true` means matching instances are skipped entirely |
| `THRESHOLD%` | Effective `usageThresholdPercent` |
| `GROW` | Effective growth: `percent +N%` or `absolute +NGiB` |
| `MAX_GIB` | Effective `maxVolumeSizeGiB` ceiling |

With `--count`, it also discovers target instances via AWS and appends a
`MATCHED` column with the number of instances each policy identifies (equals
the `external_ebs_autoresizer_policy_instances` metric):

```console
$ external-ebs-autoresizer policies --count --config config.example.yaml
POLICY   WEIGHT  SELECTOR                        PAUSED  THRESHOLD%  GROW             MAX_GIB  MATCHED
bastion  5       name~bastion                    true    60          percent +10%     1000     1
shared   1       name~^shared-                   false   80          absolute +50GiB  1000     5
default  -       (instances matching no policy)  false   80          percent +10%     1000     0
```

### instances

Discovers target instances exactly as the controller does (`tagFilters`,
`excludeEKSNodes`) and lists every instance grouped by the policy it matches.
Requires AWS credentials with `ec2:DescribeInstances` and
`ec2:DescribeVolumes`; makes no writes. A policy that matches nothing shows
`(none)` so a selector that silently stopped matching is visible.

```console
$ external-ebs-autoresizer instances --config config.example.yaml
POLICY   INSTANCE_ID          NAME           ROOT_VOLUME            SIZE_GIB
bastion  i-0a1b2c3d4e5f67890  bastion-01     vol-0a1b2c3d4e5f67890  30
shared   i-0123456789abcdef0  shared-web-01  vol-0123456789abcdef0  120
shared   i-0fedcba9876543210  shared-db-01   vol-0fedcba9876543210  500
default  (none)

3 instances discovered in ap-northeast-2
```

Use it to answer "which policy will govern this instance?" and to confirm a
policy's reach before merging a selector change.

### unused

Lists the PersistentVolumeClaims and PersistentVolumes no workload is using,
sorted longest-unused first. It reads the Kubernetes API and writes nothing, so
it is safe to run at any time. Requires in-cluster access, so it runs inside the
Pod rather than from a laptop.

```console
$ kubectl exec deploy/external-ebs-autoresizer -- external-ebs-autoresizer unused
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
| `REASON` | Why it is unused. See [Unused volume identification](../README.md#unused-volume-identification) for each value |
| `UNUSED_FOR` | How long it has been continuously unused |
| `CAPACITY` | Provisioned size, the number that turns the report into a cost |
| `EBS_VOLUME` | The EBS volume ID to price or delete in EC2, `-` when not EBS-backed |
| `BOUND_TO` | The bound volume for a claim, the bound claim for a volume |

The `UNUSED_FOR` clock lives in the annotation the scan loop writes, so a freshly
started controller reports every object as unused since now until its first pass
lands. Which objects are listed is unaffected. Pass `--all` to include objects
that have not yet been unused for 24 hours.

Nothing in this command deletes anything, and neither does the loop behind it.
"Unused" is an observation about the cluster's current state, not a statement
that the data is disposable: a claim held for a quarterly job and a claim nobody
will ever read again look identical from here.

### run

Starts the controller (identical to running with no subcommand): loads the
config, compiles policies, serves health and metrics, elects a leader when
enabled, and reconciles on the configured interval.

## Running inside the Pod

The container image ships the same binary, so every command works via
`kubectl exec` against the mounted ConfigMap:

```bash
kubectl exec deploy/external-ebs-autoresizer -- external-ebs-autoresizer policies --count
kubectl exec deploy/external-ebs-autoresizer -- external-ebs-autoresizer instances
kubectl exec deploy/external-ebs-autoresizer -- external-ebs-autoresizer unused
```

The Pod's IRSA/Pod Identity credentials cover the read-only EC2 calls, since
the controller already requires them. `unused` needs no AWS credentials at all,
only the ClusterRole the chart creates whenever `rbac.create` is true.

## Local verification

`config.example.yaml` at the project root mirrors the chart-rendered config.
The Makefile wraps each command against it; override the file with
`CONFIG=path`:

```bash
make validate     # validate  --config config.example.yaml
make policies     # policies   --config config.example.yaml
make instances    # instances  --config config.example.yaml (needs AWS credentials)
```

`unused` has no local equivalent: it reads the Kubernetes API through the
in-cluster config, which does not exist outside a Pod.

## Conclusion

Three read-only questions, three commands:

- Is the config valid? `validate`
- What does each policy do, and how many instances does it cover? `policies [--count]`
- Which policy governs which instance? `instances`

Wire `validate` into CI for config changes, and reach for `policies --count`
first when a resize did not happen where you expected one.
