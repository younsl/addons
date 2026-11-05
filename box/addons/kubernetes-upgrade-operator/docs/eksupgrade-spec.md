# EKSUpgrade Spec

`EKSUpgrade` is a cluster-scoped [custom resource](https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/custom-resources/) that declares the desired upgrade state for an EKS cluster. The operator watches these resources and continuously reconciles the actual cluster state to match the spec through the Kubernetes [control loop](https://kubernetes.io/docs/concepts/architecture/controller/). This enables GitOps-driven upgrades where the upgrade intent is version-controlled and auditable, and interrupted upgrades are automatically resumed without manual intervention.

## Examples

Same-account upgrade:

```yaml
apiVersion: kuo.io/v1alpha1
kind: EKSUpgrade
metadata:
  name: staging-upgrade
spec:
  clusterName: staging-cluster
  targetVersion: "1.34"
  region: ap-northeast-2
  upgradeMode: Forward
```

Cross-account upgrade with Slack notification:

```yaml
apiVersion: kuo.io/v1alpha1
kind: EKSUpgrade
metadata:
  name: production-upgrade
spec:
  clusterName: production-cluster
  targetVersion: "1.34"
  region: ap-northeast-2
  upgradeMode: Forward
  assumeRoleArn: arn:aws:iam::123456789012:role/kuo-spoke-role
  notification:
    onUpgrade: true
    onDryRun: false
```

Upgrade including Karpenter NodePool [node](https://kubernetes.io/docs/concepts/architecture/nodes/) replacement:

```yaml
apiVersion: kuo.io/v1alpha1
kind: EKSUpgrade
metadata:
  name: staging-upgrade
spec:
  clusterName: staging-cluster
  targetVersion: "1.34"
  region: ap-northeast-2
  upgradeMode: Forward
  karpenterNodePools:
    enabled: true
    nodePools: ["ALL"]       # empty or ["ALL"] = all NodePools; or list names in order
    strategy: Replace
    maxUnavailable: "1"      # integer or percentage, e.g. "10%"
    nodeDrainTimeoutMinutes: 15
    controllerStableTimeoutMinutes: 10
```

For a rollback example, see [Upgrade Modes](upgrade-modes.md#rollback).

## Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `clusterName` | Yes | - | EKS cluster name |
| `targetVersion` | Yes | - | Target Kubernetes version (e.g., `"1.34"`) |
| `region` | Yes | - | AWS region |
| `upgradeMode` | Yes | - | `Forward` to upgrade, `Rollback` to revert one minor version (N-1). Must be set explicitly |
| `assumeRoleArn` | No | - | IAM Role ARN for cross-account access |
| `addonVersions` | No | auto-resolve | Add-on version overrides (`addon-name: version`). On rollback, unpinned add-ons auto-roll-back to the target minor's default compatible version when that is a downgrade |
| `dryRun` | No | `false` | Plan only, do not execute |
| `timeouts.controlPlaneMinutes` | No | `30` | Control plane upgrade timeout |
| `timeouts.nodegroupMinutes` | No | `60` | Node group upgrade timeout |
| `notification.onUpgrade` | No | `false` | Send Slack notifications for actual upgrades (`dryRun: false`) |
| `notification.onDryRun` | No | `false` | Send Slack notifications for dry-run executions (`dryRun: true`) |
| `karpenterNodePools.enabled` | No | `false` | Enable Karpenter NodePool node replacement after the node group phase |
| `karpenterNodePools.nodePools` | No | `[]` (all) | NodePools to process, in order. Empty or `["ALL"]` (case-insensitive) means all NodePools |
| `karpenterNodePools.strategy` | No | `Replace` | Replacement strategy (v1 supports `Replace` only) |
| `karpenterNodePools.maxUnavailable` | No | `"1"` | Nodes replaced concurrently per NodePool, integer or percentage (`"10%"`) |
| `karpenterNodePools.nodeDrainTimeoutMinutes` | No | `15` | Max wait for an old node to [drain](https://kubernetes.io/docs/tasks/administer-cluster/safely-drain-node/) and be removed |
| `karpenterNodePools.controllerStableTimeoutMinutes` | No | `10` | Max wait for [evicted](https://kubernetes.io/docs/concepts/scheduling-eviction/api-eviction/) [pods](https://kubernetes.io/docs/concepts/workloads/pods/)' controllers to become Ready again |

## Status

```bash
kubectl get eksupgrades
NAME                 CLUSTER              TARGET   PHASE             PROGRESS   AUTH                AGE
staging-upgrade      staging-cluster      1.34     Completed         4/4        IdentityVerified    5m
production-upgrade   production-cluster   1.34     UpgradingAddons   2/5        AssumeRoleSuccess   2m
```

## Conclusion

One cluster-scoped `EKSUpgrade` per cluster declares the target version and mode, and every other field is an opt-in or a timeout with a working default. Commit the resource to Git, let kuo reconcile it, and follow progress through `kubectl get eksupgrades`.
