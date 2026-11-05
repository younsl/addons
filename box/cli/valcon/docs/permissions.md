# Permissions

The AWS and Kubernetes permissions valcon needs, for administrators granting access.

## AWS

`elasticache:DescribeReplicationGroups`, `ec2:DescribeNetworkInterfaces`, `cloudwatch:GetMetricData`.

## Kubernetes

In the scanned namespaces: `list` and `get` on `pods`, `create` on `pods/exec`, and `patch` on `pods/ephemeralcontainers` for database browsing.

## Conclusion

Three read-only AWS actions cover the groups screen. Kubernetes exec covers connected pods, and `pods/ephemeralcontainers` is needed only for key browsing.
