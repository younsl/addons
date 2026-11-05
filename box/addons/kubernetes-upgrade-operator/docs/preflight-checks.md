# Preflight Checks

Every run, dry runs included, passes the `PreflightChecking` phase before any infrastructure change. A failed mandatory check moves the run to `Failed`.

| Check | Category | Behavior |
|-------|----------|----------|
| EKS Cluster Insights | Mandatory | Fails if critical insights exist |
| [EKS Deletion Protection](https://docs.aws.amazon.com/eks/latest/userguide/delete-cluster.html) | Mandatory | Fails if deletion protection is disabled |
| PDB Drain Deadlock | Mandatory | Fails if any [PDB](https://kubernetes.io/docs/concepts/workloads/pods/disruptions/) has `disruptionsAllowed == 0`, naming the blocking `namespace/name` PDBs (always enforced) |
| Karpenter v1 API | Mandatory | Only when `karpenterNodePools.enabled`. Fails if the Karpenter v1 API is not served (v1beta1 and earlier unsupported) |
| Karpenter AMI Selector | Mandatory | Only when `karpenterNodePools.enabled`. Fails if a target NodePool pins its AMI instead of `alias: <family>@latest` |

Rollback runs query insights under the `ROLLBACK_READINESS` category instead of `UPGRADE_READINESS`.

## Reported resources

Each check records the objects it flagged in `status.phases.preflight.checks[].resources`. The per-check `message` states only why the check failed, and the names live in `resources`:

```yaml
status:
  # Flat top-level message (also sent to Slack) inlines the names, since it has
  # no structured counterpart.
  message: "Preflight check failed: [PDB Drain Deadlock] 2 of 12 PodDisruptionBudgets block node drain because they allow zero disruptions: payments/worker-pdb, kube-system/coredns-pdb"
  phases:
    preflight:
      checks:
        - name: PDB Drain Deadlock
          status: Fail
          message: "2 of 12 PodDisruptionBudgets block node drain because they allow zero disruptions"
          resources:
            - payments/worker-pdb
            - kube-system/coredns-pdb
```

Resource formats: `namespace/name` for PDBs, NodePool name for the Karpenter AMI check, `type:id` for EKS Insights findings (ERROR and CRITICAL only).

## Logs

Operator logs print the same result as full sentences, one line per flagged resource:

```
Preflight check PDB Drain Deadlock failed. 2 of 12 PodDisruptionBudgets block node drain because they allow zero disruptions.
Preflight check PDB Drain Deadlock flagged payments/worker-pdb as resource 1 of 2.
Preflight check PDB Drain Deadlock flagged kube-system/coredns-pdb as resource 2 of 2.
```

## Conclusion

Every current check is mandatory, and a failure stops the run in `Failed` before kuo changes anything, dry runs included. The blocking objects are listed in `status.phases.preflight.checks[].resources` and in the operator log, so the fix starts from a concrete PDB, NodePool, or insight rather than from a generic error.
