---
description: Handle Spot and other involuntary EC2 interruptions with Karpenter's native EventBridge to SQS interruption queue instead of AWS Node Termination Handler.
tags: [kubernetes, karpenter, spot, eks, aws]
resources: [NodePool, EC2NodeClass]
status: adopted
reviewed: 2026-10-10
---

# Use Karpenter interruption handling instead of NTH

## Rule

On clusters where Karpenter provisions the nodes, interruption handling belongs to Karpenter itself. EventBridge rules forward EC2 and AWS Health events to an SQS queue, and Karpenter reads that queue through `settings.interruptionQueue`. AWS Node Termination Handler (NTH) is not installed.

- Create one SQS queue per cluster and the EventBridge rules below, all targeting that queue
- Grant the Karpenter controller role `sqs:ReceiveMessage`, `sqs:DeleteMessage`, and `sqs:GetQueueUrl` on the queue
- Remove NTH when migrating to Karpenter, never run both against the same nodes

| Event | Source | Detail type |
|-------|--------|-------------|
| Spot interruption | `aws.ec2` | `EC2 Spot Instance Interruption Warning` |
| Rebalance recommendation | `aws.ec2` | `EC2 Instance Rebalance Recommendation` |
| Instance stopping or terminating | `aws.ec2` | `EC2 Instance State-change Notification` |
| Scheduled maintenance | `aws.health` | `AWS Health Event` |
| Capacity reservation interruption | `aws.ec2` | `EC2 Capacity Reservation Instance Interruption Warning` |

## Why

- **One owner for the node lifecycle**: Karpenter already taints, drains, and terminates nodes. With interruption handling it does the same on an interruption notice and starts the replacement node immediately, using as much of the two-minute Spot notice as possible for pod cleanup.
- **Smarter replacement**: on a Spot interruption, Karpenter marks that offering (instance type and zone) unavailable in its cache, so the replacement avoids the pool that is being reclaimed. NTH cannot influence what gets launched next.
- **No conflicting actors**: Karpenter and NTH share no state. When NTH removes a node, Karpenter sees only lost capacity, may launch the same instance type again, and the cycle repeats as very short-lived nodes. EKS Best Practices advises against running both.
- **Less to operate**: NTH needs either a DaemonSet on every node (IMDS mode) or its own Deployment, IAM role, and the same SQS and EventBridge setup (queue mode). Native handling reuses the queue and drops the extra controller, chart, and patching.
- **Wider event coverage**: one queue covers Spot interruptions, scheduled maintenance, stop and terminate state changes, instance status check failures, and capacity reservation interruptions.
- **Project direction**: NTH is not archived and still ships patch releases, but activity has wound down (about 90 commits a year in 2022 and 2023, 27 in 2025, 10 so far in 2026, and no minor release since v1.25.0 in February 2025). Karpenter is where AWS develops node lifecycle handling now.

## Exceptions

- Karpenter records Spot rebalance recommendations as node events but does not drain on them. If proactive draining on rebalance is truly required, NTH can run alongside, at the cost of the churn described above. The two-minute interruption notice is enough for most workloads
- Nodes Karpenter does not manage are out of scope. EKS managed node groups handle Spot rebalancing themselves, and self-managed Auto Scaling groups still need their own handling

## Example

Karpenter Helm values:

```yaml
settings:
  clusterName: my-cluster
  interruptionQueue: my-cluster-karpenter
```

The `karpenter` submodule of `terraform-aws-modules/eks` creates the queue, queue policy, and all the rules above by default (`enable_spot_termination = true`). Its `queue_name` output feeds `interruptionQueue`.

Confirm the queue is wired up and drained:

```bash
aws sqs get-queue-attributes --queue-url "$(aws sqs get-queue-url --queue-name my-cluster-karpenter --query QueueUrl --output text)" --attribute-names ApproximateNumberOfMessages
```

## References

- [Karpenter: Interruption](https://karpenter.sh/docs/concepts/disruption/#interruption)
- [Karpenter Troubleshooting: AWS Node Termination Handler (NTH) interactions](https://karpenter.sh/docs/troubleshooting/#aws-node-termination-handler-nth-interactions)
- [EKS Best Practices: Enable interruption handling when using Spot](https://docs.aws.amazon.com/eks/latest/best-practices/karpenter.html#_enable_interruption_handling_when_using_spot)
- [terraform-aws-eks karpenter module](https://github.com/terraform-aws-modules/terraform-aws-eks/tree/master/modules/karpenter)
- [aws-node-termination-handler releases](https://github.com/aws/aws-node-termination-handler/releases)
