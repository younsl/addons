# Test results

Where and how the images were verified.

Verified in production on [Amazon EKS 1.36](https://docs.aws.amazon.com/eks/latest/userguide/kubernetes-versions-standard.html#kubernetes-1-36) with Graviton `m7g.2xlarge` nodes, running v2.15.3 through harbor-helm 1.19.2 with an external PostgreSQL and S3 registry storage. Every component reported healthy, the 2.15.3 schema migration applied cleanly, and image pulls through the registry succeeded with no errors.

## Conclusion

v2.15.3 runs healthy in production on EKS Graviton nodes, migrations and image pulls included.
