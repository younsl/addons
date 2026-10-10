---
description: Handle Spot and other involuntary EC2 interruptions with Karpenter's native EventBridge to SQS interruption queue instead of AWS Node Termination Handler.
tags: [kubernetes, karpenter, spot, eks, aws]
resources: [NodePool, EC2NodeClass]
status: adopted
reviewed: 2026-10-10
---

# Use Karpenter interruption handling instead of NTH

## Rule

On clusters where [Karpenter](https://karpenter.sh/) provisions the nodes, [interruption handling](https://karpenter.sh/docs/concepts/disruption/#interruption) belongs to Karpenter itself. [EventBridge rules](https://docs.aws.amazon.com/eventbridge/latest/userguide/eb-rules.html) forward EC2 and AWS Health events to an [SQS](https://docs.aws.amazon.com/AWSSimpleQueueService/latest/SQSDeveloperGuide/welcome.html) queue, and Karpenter reads that queue through [`settings.interruptionQueue`](https://karpenter.sh/docs/reference/settings/). [AWS Node Termination Handler](https://github.com/aws/aws-node-termination-handler) (NTH) is not installed.

- Create one SQS queue per cluster and the EventBridge rules below, all targeting that queue
- Grant the Karpenter controller role `sqs:ReceiveMessage`, `sqs:DeleteMessage`, and `sqs:GetQueueUrl` on the queue, as in the [Karpenter CloudFormation reference](https://karpenter.sh/docs/reference/cloudformation/)
- Remove NTH when migrating to Karpenter, never run both against the same nodes

| Event | Source | Detail type |
|-------|--------|-------------|
| [Spot interruption](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/spot-instance-termination-notices.html) | `aws.ec2` | `EC2 Spot Instance Interruption Warning` |
| [Rebalance recommendation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/rebalance-recommendations.html) | `aws.ec2` | `EC2 Instance Rebalance Recommendation` |
| [Instance stopping or terminating](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/monitoring-instance-state-changes.html) | `aws.ec2` | `EC2 Instance State-change Notification` |
| [Scheduled maintenance](https://docs.aws.amazon.com/health/latest/ug/cloudwatch-events-health.html) | `aws.health` | `AWS Health Event` |
| [Capacity reservation interruption](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/interruptible-capacity-reservations.html) | `aws.ec2` | `EC2 Capacity Reservation Instance Interruption Warning` |

## Why

- **One owner for the node lifecycle**: Karpenter already taints, drains, and terminates nodes. With interruption handling it does the same on an interruption notice and starts the replacement node immediately, using as much of the two-minute [Spot](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/using-spot-instances.html) notice as possible for pod cleanup.
- **Smarter replacement**: on a Spot interruption, Karpenter marks that offering (instance type and zone) unavailable in its cache, so the replacement avoids the pool that is being reclaimed. NTH cannot influence what gets launched next.
- **No conflicting actors**: Karpenter and NTH share no state. When NTH removes a node, Karpenter sees only lost capacity, may launch the same instance type again, and the cycle repeats as very short-lived nodes. [EKS Best Practices](https://docs.aws.amazon.com/eks/latest/best-practices/karpenter.html#_enable_interruption_handling_when_using_spot) advises against running both.
- **Less to operate**: NTH needs either a [DaemonSet](https://kubernetes.io/docs/concepts/workloads/controllers/daemonset/) on every node (IMDS mode) or its own Deployment, IAM role, and the same SQS and EventBridge setup (queue mode). Native handling reuses the queue and drops the extra controller, chart, and patching.
- **Wider event coverage**: one queue covers Spot interruptions, scheduled maintenance, stop and terminate state changes, [instance status check](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/monitoring-system-instance-status-check.html) failures, and capacity reservation interruptions.
- **Project direction**: NTH is not archived and still ships patch releases, but activity has wound down (about 90 commits a year in 2022 and 2023, 27 in 2025, 10 so far in 2026, and no minor release since [v1.25.0](https://github.com/aws/aws-node-termination-handler/releases/tag/v1.25.0) in February 2025). Karpenter is where AWS develops node lifecycle handling now. The maintenance status question is open as [aws/aws-node-termination-handler#1286](https://github.com/aws/aws-node-termination-handler/issues/1286).

## Exceptions

- Karpenter records Spot rebalance recommendations as node events but does not drain on them. If proactive draining on rebalance is truly required, NTH can run alongside, at the cost of the churn described in [NTH interactions](https://karpenter.sh/docs/troubleshooting/#aws-node-termination-handler-nth-interactions). The two-minute interruption notice is enough for most workloads
- Nodes Karpenter does not manage are out of scope. [EKS managed node groups](https://docs.aws.amazon.com/eks/latest/userguide/managed-node-groups.html#managed-node-group-capacity-types) handle Spot rebalancing themselves, and [self-managed nodes](https://docs.aws.amazon.com/eks/latest/userguide/worker.html) still need their own handling

## Example

Karpenter Helm values:

```yaml
settings:
  clusterName: my-cluster
  interruptionQueue: my-cluster-karpenter
```

The [`karpenter` submodule](https://github.com/terraform-aws-modules/terraform-aws-eks/tree/master/modules/karpenter) of `terraform-aws-modules/eks` creates the queue, queue policy, and all the rules above by default (`enable_spot_termination = true`). Its `queue_name` output feeds `interruptionQueue`.

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
