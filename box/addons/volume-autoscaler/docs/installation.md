# Installation

The AWS and cluster prerequisites, the IAM policy, and the Helm install. Read this before the first install.

## Prerequisites

Before installing, set up AWS authentication. The addon authenticates to AWS
through [EKS Pod Identity][pod-identity], so the following must already exist:

1. An IAM role with the policy from the [IAM](#iam) section attached, and a
   trust policy that allows the `pods.eks.amazonaws.com` service principal.
2. The [EKS Pod Identity Agent][pod-identity-agent] add-on installed on the
   cluster.
3. An [EKS Pod Identity association][pod-identity-assoc] that maps the role to
   the addon's ServiceAccount (`volume-autoscaler` in the `kube-system`
   namespace by default). Create it after the chart is installed, or pre-create
   the ServiceAccount and reuse it.

With Pod Identity the role mapping lives in the association, so no
`eks.amazonaws.com/role-arn` [annotation][k8s-annotations] is needed on the ServiceAccount.


## IAM

Attach this policy to the addon's IAM role. The role is mapped to the addon's
ServiceAccount through an EKS Pod Identity association (see Prerequisites).
`Describe*` actions do not support resource-level permissions and require `"*"`;
`ec2:ModifyVolume` is scoped to volumes and `ssm:SendCommand` to the managed
document and instances.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "DiscoverInstancesAndVolumes",
      "Effect": "Allow",
      "Action": [
        "ec2:DescribeInstances",
        "ec2:DescribeVolumes",
        "ec2:DescribeVolumesModifications"
      ],
      "Resource": "*"
    },
    {
      "Sid": "ModifyRootVolume",
      "Effect": "Allow",
      "Action": "ec2:ModifyVolume",
      "Resource": "arn:aws:ec2:*:123456789012:volume/*"
    },
    {
      "Sid": "RunResizeCommandsViaSSM",
      "Effect": "Allow",
      "Action": "ssm:SendCommand",
      "Resource": [
        "arn:aws:ssm:*::document/AWS-RunShellScript",
        "arn:aws:ec2:*:123456789012:instance/*"
      ]
    },
    {
      "Sid": "ReadSSMCommandResults",
      "Effect": "Allow",
      "Action": [
        "ssm:GetCommandInvocation",
        "ssm:DescribeInstanceInformation"
      ],
      "Resource": "*"
    },
    {
      "Sid": "ReadInstanceTypeEBSCapabilitiesForThroughputRecommendations",
      "Effect": "Allow",
      "Action": "ec2:DescribeInstanceTypes",
      "Resource": "*"
    }
  ]
}
```

Every instance an InstanceVolumeAutoscaler targets is resized, so the IAM role is the outer bound of what the addon can change. To keep it inside a known set of instances whatever the autoscalers say, add a tag condition to the two mutating statements, for example `"Condition": {"StringEquals": {"aws:ResourceTag/volume-autoscaler": "enabled"}}` on `ModifyRootVolume` (volume tags) and `ssm:resourceTag/volume-autoscaler` on the instance in `RunResizeCommandsViaSSM`. See [Who can create an autoscaler](instance-volume-autoscaler.md#who-can-create-an-autoscaler).

`ec2:DescribeInstanceTypes` is only needed when `throughputRecommendation.enabled`
is true; it reads the instance type's EBS bandwidth ceiling. That feature also needs
`get`, `list`, and `patch` on `nodes` in the addon's [ClusterRole][k8s-rbac].

The [protective auto-cordon][k8s-cordon] needs no IAM permission. It needs the same `nodes` grant plus `patch` on `nodes/status` for its `ProtectiveCordon` condition, which the chart always grants.

Replace `123456789012` with your account ID. To restrict which instances can be
modified or commanded, narrow the `instance/*` and `volume/*` ARNs or add a
`Condition` on `aws:ResourceTag`.

Target instances must have the SSM Agent running and the
`AmazonSSMManagedInstanceCore` managed policy attached.

## Install

Complete the [Prerequisites](#prerequisites) and [IAM](#iam) steps first, then install the chart.

The recommended way to install is the Helm chart published as an OCI artifact on
GHCR. Installing into the `kube-system` namespace is recommended, since this is a
cluster-level addon.

```bash
helm upgrade --install volume-autoscaler \
  oci://ghcr.io/younsl/charts/volume-autoscaler \
  --version x.y.z \
  --namespace kube-system \
  --set config.region=ap-northeast-2
```

Replace x.y.z with a released chart version. List them with [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/README.md):

```bash
crane ls ghcr.io/younsl/charts/volume-autoscaler
```

To install from a local checkout instead, point Helm at the chart directory:

```bash
helm upgrade --install volume-autoscaler ./charts/volume-autoscaler \
  --namespace kube-system \
  --set config.region=ap-northeast-2
```

The chart installs the InstanceVolumeAutoscaler CRD (`crds.install`, kept on uninstall by `crds.keep`) and grants the addon read access to autoscalers and patch access to their status. Nothing is resized until an autoscaler targets an instance, so apply one next, or render them from the chart's `instanceVolumeAutoscalers` value. See [InstanceVolumeAutoscaler](instance-volume-autoscaler.md).

```bash
kubectl apply -f examples/
kubectl get iva
```

Observability:
- `/healthz`, `/readyz` on `:8080`
- Prometheus `/metrics` on `:8081`

See [metrics.md](metrics.md) for the full list of exposed metrics,
their labels, and example PromQL queries. See
[alerting.md](alerting.md) for how alerts are pushed to Alertmanager,
including alert types, labels, the notify-on policy, and routing examples. See
[grafana-annotations.md](grafana-annotations.md) for marking resize
events on Grafana dashboards.

[pod-identity]: https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html
[pod-identity-agent]: https://docs.aws.amazon.com/eks/latest/userguide/pod-id-agent-setup.html
[pod-identity-assoc]: https://docs.aws.amazon.com/eks/latest/userguide/pod-id-association.html

[k8s-annotations]: https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/
[k8s-rbac]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#role-and-clusterrole
[k8s-cordon]: https://kubernetes.io/docs/concepts/architecture/nodes/#manual-node-administration

## Conclusion

Before the first install, create an IAM role with the policy above, install the EKS Pod Identity Agent, associate the role with the `volume-autoscaler` ServiceAccount, and make sure every target instance runs the SSM Agent with `AmazonSSMManagedInstanceCore` attached. Then install the OCI chart into `kube-system` at a version listed by `crane ls`, and apply an InstanceVolumeAutoscaler for every instance group to grow. The throughput recommender additionally needs `ec2:DescribeInstanceTypes`, while the protective auto-cordon needs no IAM permission at all.
