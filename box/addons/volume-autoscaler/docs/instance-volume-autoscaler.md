# InstanceVolumeAutoscaler

How to declare which EC2 instances get their root volume grown, and how far. Read this before writing an InstanceVolumeAutoscaler, or when migrating from the retired `defaultPolicy` and `policies` config keys.

## Overview

An InstanceVolumeAutoscaler (short name `iva`) is a cluster-scoped custom resource modeled on the [HorizontalPodAutoscaler][k8s-hpa]. Where an HPA scales the replicas of a workload on CPU utilization, an InstanceVolumeAutoscaler scales the root EBS volume capacity of EC2 instances on root filesystem utilization. Only instances an autoscaler targets are ever resized. Every other discovered instance is left alone.

```yaml
apiVersion: autoscaling.volume-autoscaler.sh/v1alpha1
kind: InstanceVolumeAutoscaler
metadata:
  name: database
spec:
  scaleTargetRef:
    kind: TagSelector
    tags:
      Role: database
  maxCapacity: 2Ti
  metrics:
    - type: Resource
      resource:
        name: storage
        target:
          type: Utilization
          averageUtilization: 70
  behavior:
    scaleUp:
      selectPolicy: Max
      policies:
        - type: Percent
          value: 20
        - type: GiB
          value: 50
```

More examples live in [examples/instance.yaml](../examples/instance.yaml) (one instance by ID) and [examples/tag-selector.yaml](../examples/tag-selector.yaml) (instances by tags). The chart can render autoscalers from its `instanceVolumeAutoscalers` value.

## Volumes only grow

> [!WARNING]
> Unlike an HPA, an InstanceVolumeAutoscaler has no scale down. Every resize is permanent, so plan `maxCapacity` before you apply one.

- EBS can only increase a volume's size. [`ModifyVolume`][ec2-modifyvolume] rejects a smaller size ([volume modification requirements][ebs-modify-reqs]), and the filesystem is grown in place. XFS cannot shrink at all, and ext4 only offline.
- The added capacity is billed until the volume is deleted, even after the data that triggered the resize is cleaned up.
- Getting capacity back takes a manual migration: create a smaller volume, copy the data, and swap the root device, with downtime for the instance.
- Lowering `maxCapacity` or deleting the autoscaler stops further growth but never returns capacity.
- Set `maxCapacity` to the largest size you are willing to pay for. Treat repeated `SucceededRescale` Events on one instance as a signal to find what fills the disk (logs, caches, temporary files), not only as a reason to let it grow.

## Who can create an autoscaler

The controller resizes whatever an autoscaler targets, within the reach of its IAM role. Creating or editing an InstanceVolumeAutoscaler therefore lets a user have the controller grow volumes (and their bill) and run its fixed resize script as root over SSM on any instance that role can reach, and cordon any targeted Node.

- Grant `create`, `update`, and `patch` on `instancevolumeautoscalers` only to cluster administrators or the GitOps controller. The CRD is cluster-scoped, so a namespaced Role cannot grant it.
- Bound what the controller can touch in IAM rather than in the autoscaler, for example with a tag condition on `ec2:ModifyVolume` and `ssm:SendCommand` (see [IAM](installation.md#iam)).

## Mapping to the HPA

| HorizontalPodAutoscaler | InstanceVolumeAutoscaler |
| --- | --- |
| `scaleTargetRef` (a Deployment) | `scaleTargetRef` (one instance by ID, or instances by tags) |
| `maxReplicas` | `maxCapacity` |
| `metrics[].resource` (`cpu`, `memory`) | `metrics[].resource` (`storage`, root filesystem usage) |
| `behavior.scaleUp.policies` (`Pods`, `Percent`) | `behavior.scaleUp.policies` (`GiB`, `Percent`) |
| `behavior.scaleUp.selectPolicy` (`Max`, `Min`, `Disabled`) | same |
| `behavior.scaleDown` | none, an EBS volume never shrinks (see [Volumes only grow](#volumes-only-grow)) |
| `periodSeconds`, `stabilizationWindowSeconds` | `behavior.scaleUp.cooldownDuration` (from the gardener pvc-autoscaler), at least 6h |
| Conditions `AbleToScale`, `ScalingActive`, `ScalingLimited` | same |
| Events `SuccessfulRescale`, `FailedRescale` | same |

## Spec

- `scaleTargetRef.kind: Instance` with `name` targets one instance by ID (`i-0123456789abcdef0`).
- `scaleTargetRef.kind: TagSelector` with `tags` targets every discovered instance carrying all listed tags with the exact values. The Name tag works like any other tag.
- `maxCapacity` (required) is a [quantity][k8s-quantity] such as `500Gi`, `2Ti`, `500G`, or `1e12`, rounded up to whole GiB. A step that would exceed it is not taken.
- `metrics` holds at most one `storage` utilization target. Omitted, the target is 80 percent, as an HPA without metrics defaults to 80 percent CPU.
- `behavior.scaleUp.policies` lists step sizes. `Percent` is a share of the current capacity, rounded up to whole GiB. `GiB` is a fixed amount. Omitted, the policies are 10 percent and 1 GiB.
- `behavior.scaleUp.selectPolicy` picks the largest step (`Max`, the default) or the smallest (`Min`). `Disabled` stops the autoscaler from measuring or resizing its instances without deleting it.
- `behavior.scaleUp.cooldownDuration` is the minimum time between two modifications of the same volume, written in hours, minutes, and seconds (`6h`, `6h30m`, `390m`). It cannot be shorter than `6h`, which is also the default. [EBS accepts four modifications per volume in a rolling 24 hours][ebs-modify-limit], and modifications at least six hours apart never exceed that.

  > You can modify a volume up to four times within a rolling 24-hour period, as long as the volume is in the in-use or available state, and all previous modifications for that volume are completed.
  >
  > [Modify a volume using Amazon EBS Elastic Volumes][ebs-modify-limit]

- The floor exists because the EC2 API cannot tell how many times a volume was modified in the last 24 hours. [`DescribeVolumesModifications`][ec2-describevolumesmodifications] returns only the most recent modification of each volume, so the controller cannot count modifications against the limit, and a shorter cooldown could send a fifth [`ModifyVolume`][ec2-modifyvolume] that EBS rejects.
- The `6h` floor is temporary. Once the EC2 API reports how many of the four modification slots a volume has used in the rolling 24 hours, or when the next slot frees up, the floor will be lifted. The controller will then check the remaining slots before each modification, so a fast-filling volume can grow again as soon as its previous modification completes.
- `alertEnabled` (default true) mutes Alertmanager alerts for this autoscaler when false. The controller-wide `alertmanager.enabled` switch remains the master gate.

Every step grows the volume by at least 1 GiB.

## Validation

The CRD schema and CEL rules (`x-kubernetes-validations`) reject an invalid spec at `kubectl apply`, so no admission webhook, certificate, or extra Deployment is involved. The API server rejects:

- a `scaleTargetRef` that mixes `name` and `tags`, a `TagSelector` without tags, and an instance ID that is not `i-` followed by 8 to 17 hex digits
- an empty tag key or value, a key over 128 or a value over 256 characters, and more than 50 tags (the EC2 tag limits)
- a `maxCapacity` that is not a quantity (`10GB`) or is under `1Gi`
- a `cooldownDuration` outside `h`, `m`, and `s` units (`1d`, `1.5h`) or shorter than `6h`
- an unsupported metric, target, or policy type, a utilization outside 1 to 100, a step under 1, and more than one metric

It defaults `alertEnabled` to true and `selectPolicy` to `Max`. The controller runs the same checks again and marks a spec that slips past them (for example one stored before a CRD upgrade) with `ScalingActive=False` and reason `InvalidSpec`. Overlapping autoscalers involve more than one object, which CEL cannot see, so they are reported as `AmbiguousSelector` rather than rejected at admission, as with overlapping HPAs.

## Overlapping autoscalers

When two autoscalers target the same instance, both refuse to scale any of their instances, the way overlapping HPAs do. Each reports `ScalingActive=False` with reason `AmbiguousSelector`, lists the other autoscaler in the message, and gets a Warning Event. Remove the overlap to resume. Use disjoint tags, or move the instance to its own `kind: Instance` autoscaler and drop the matching tag.

## Status

`kubectl get iva` keeps to what an on-call engineer needs at a glance. The scale-up rules stay in `kubectl describe iva`.

```console
$ kubectl get iva
NAME       TARGET                  STATUS              VOLUMES  UTILIZATION  MAXSIZE  LASTSCALE  AGE
bastion    i-0123456789abcdef0     Ready               1        64%/80%      50Gi                30d
database   Env=prod,Role=database  Cooldown            3        92%/70%      2Ti      2h         30d
web        Role=web                MaxCapacityReached  2        95%/80%      200Gi    5d         30d
```

- `TARGET` is the scale target in one line: the instance ID, or the required tags.
- `STATUS` is the condition reason to look at first: an inactive autoscaler (`ScalingActive`), then one unable to grow (`AbleToScale`), then one at its ceiling (`ScalingLimited`). `Ready` means all three are healthy.
- `VOLUMES` is how many root EBS volumes the autoscaler targets, the unit it scales, as `REPLICAS` counts Pods on an HPA. It includes volumes held back as ambiguous or disabled, and leaves out an instance with no resolvable root volume.
- `UTILIZATION` is the fullest target disk against the utilization target, like the HPA `TARGETS` column. It shows `<unknown>` until a target is measured.
- `MAXSIZE` is `spec.maxCapacity`, the ceiling a step never passes. Each volume's current size is in `status.instances[].currentCapacity`.
- `LASTSCALE` is when any target volume last grew.

The spec validator fills `TARGET`, `UTILIZATION`, and `STATUS` as soon as an autoscaler is applied, so a new or rejected autoscaler never shows blank columns. The leader rewrites the whole status at the end of every resize pass.

- `currentVolumes` counts the targeted root volumes, and `instances[]` lists every targeted instance with `currentCapacity`, `desiredCapacity`, `currentUtilization`, `lastScaleTime`, and a `reason`.
- `lastScaleTime` is the latest resize across all targets.
- `cooldownDuration` is the cooldown in effect, the `6h` default included, and each `instances[].cooldownUntil` is when that volume may be modified again. It is set right after a resize and while a cooldown blocks a needed one.
- `observedGeneration` is the spec generation the status was computed from.

Per-instance reasons:

- `BelowTarget`: usage is under the target, nothing to do.
- `SucceededRescale`: the volume and filesystem were grown this pass.
- `Cooldown`: the volume was modified within `cooldownDuration`, and `cooldownUntil` says when it may grow next.
- `MaxCapacityReached`: the next step would exceed `maxCapacity`.
- `DryRun`: `dryRun` is on, `desiredCapacity` shows what would happen.
- `ScalingDisabled`: `selectPolicy` is `Disabled`.
- `AmbiguousSelector`: another autoscaler targets the same instance.
- `FailedGetResourceMetric`: root usage could not be measured over SSM.
- `FailedRescale`: `ModifyVolume`, the optimizing wait, or the filesystem resize failed.
- `NoRootVolume`: the instance has no resolvable root EBS volume.

Conditions:

- `ScalingActive` is `True` (`ValidMetricFound`) while targets are measured. It is `False` with, in order of precedence, `InvalidSpec`, `ScalingDisabled`, `FailedGetScale` (instance discovery failed), `AmbiguousSelector`, `NoMatchingInstances`, or `FailedGetResourceMetric` (no target could be measured).
- `AbleToScale` is `False` with `FailedRescale` or `Cooldown` when a target that needs to grow cannot, otherwise `True` (`ReadyForNewScale` or `SucceededRescale`).
- `ScalingLimited` is `True` with `MaxCapacityReached` when a target hit the ceiling, otherwise `False` (`DesiredWithinRange`).

A spec the controller rejects shows `ScalingActive=False` with `InvalidSpec` as soon as it is applied, without waiting for the next pass.

```bash
kubectl get iva
kubectl describe iva database
kubectl get events --field-selector involvedObject.kind=InstanceVolumeAutoscaler -n default
```

## Migrating from config policies

The `defaultPolicy` and `policies` config keys are retired, and a config file that still carries them fails at startup with a pointer here. Translate each entry into an autoscaler.

- `instanceSelector.tags` becomes `scaleTargetRef.kind: TagSelector` with `tags`. A Name regex has no equivalent. Tag the instances, or target each one by ID.
- `usageThresholdPercent` becomes `metrics[0].resource.target.averageUtilization`.
- `growMode: percent` with `growPercent` becomes a `Percent` policy. `growMode: absolute` with `growAmount` becomes a `GiB` policy.
- `maxVolumeSizeGiB` becomes `maxCapacity` with a `Gi` suffix.
- `paused: true` becomes `behavior.scaleUp.selectPolicy: Disabled`.
- `alertEnabled` keeps its name.
- `weight` has no equivalent, since overlapping autoscalers are refused instead of ranked.
- `defaultPolicy` has no equivalent. Instances no autoscaler targets are not resized, so give the former default group a tag and a `TagSelector` autoscaler.

[k8s-hpa]: https://kubernetes.io/docs/tasks/run-application/horizontal-pod-autoscale/
[k8s-quantity]: https://kubernetes.io/docs/reference/kubernetes-api/common-definitions/quantity/
[ec2-modifyvolume]: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_ModifyVolume.html
[ec2-describevolumesmodifications]: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_DescribeVolumesModifications.html
[ebs-modify-limit]: https://docs.aws.amazon.com/ebs/latest/userguide/ebs-modify-volume.html
[ebs-modify-reqs]: https://docs.aws.amazon.com/ebs/latest/userguide/modify-volume-requirements.html

## Conclusion

An InstanceVolumeAutoscaler names its instances with `scaleTargetRef`, grows their root volume by the largest (or smallest) `scaleUp` policy step once root usage reaches the `storage` target, and stops at `maxCapacity`. Status and Events follow the HPA, so `kubectl get iva` and `kubectl describe iva` answer what is targeted, what grew, and why something did not.
