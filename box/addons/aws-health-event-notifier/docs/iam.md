# IAM permissions

The daemon needs read access to the AWS Health API, plus account identity for the message header. Attach this policy to the role associated with the ServiceAccount. [EKS Pod Identity](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html) is strongly recommended over IRSA.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "AwsHealthRead",
      "Effect": "Allow",
      "Action": [
        "health:DescribeEvents",
        "health:DescribeEventDetails",
        "health:DescribeEventTypes",
        "health:DescribeAffectedEntities"
      ],
      "Resource": "*"
    },
    {
      "Sid": "AccountIdentityForMessageHeader",
      "Effect": "Allow",
      "Action": [
        "sts:GetCallerIdentity",
        "iam:ListAccountAliases"
      ],
      "Resource": "*"
    }
  ]
}
```

> The AWS Health API requires a Business, Enterprise On-Ramp, or Enterprise Support plan. See [Getting started with the AWS Health API](https://docs.aws.amazon.com/health/latest/ug/health-api.html).
