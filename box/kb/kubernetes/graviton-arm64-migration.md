---
description: Move workloads to Graviton by publishing multi-arch images first, pinning arm64 scheduling per environment, and dropping to single arm64 builds only after every environment runs on arm64.
tags: [kubernetes, aws, graviton, arm64, ci, cost]
resources: [Pod, NodePool]
status: adopted
reviewed: 2026-10-10
---

# Migrate workloads to Graviton

## Rule

Migrate to [Graviton](https://aws.amazon.com/ec2/graviton/) in this order: image, scheduling, then build simplification. Each step is safe on its own because the earlier one already covers both architectures.

- **Images first**: publish a [multi-platform](https://docs.docker.com/build/building/multi-platform/) manifest list with `linux/amd64` and `linux/arm64` under the existing tag before any pod is scheduled on arm64. Clusters mix both architectures during the migration, so every image is multi-arch or carries an explicit architecture constraint
- **Native builds**: build each architecture on a runner of that architecture (for example a [GitLab runner tag](https://docs.gitlab.com/ci/yaml/#tags) `arm64`) and join the per-architecture tags with [`docker buildx imagetools create`](https://docs.docker.com/reference/cli/docker/buildx/imagetools/create/). Never compile under [QEMU emulation](https://docs.docker.com/build/building/multi-platform/#qemu)
- **Architecture-neutral Dockerfiles**: no `GOARCH=amd64`, `FROM --platform=linux/amd64`, or `x86_64` download URLs. Use the [`TARGETARCH`](https://docs.docker.com/build/building/variables/#multi-platform-build-arguments) build argument instead
- **Explicit scheduling**: pin arm64 per environment with a [`kubernetes.io/arch`](https://kubernetes.io/docs/reference/labels-annotations-taints/#kubernetes-io-arch) `nodeSelector`, or a Karpenter [NodePool requirement](https://karpenter.sh/docs/concepts/nodepools/#well-known-labels) on the same label
- **Single arm64 build last**: drop the amd64 job and the manifest list only when every environment the pipeline deploys to passes all three checks below. Otherwise a pod still placed on amd64 fails with `no match for platform in manifest`

| Check | Pass condition |
|-------|----------------|
| Running pods | Every pod of the app sits on a node labeled `kubernetes.io/arch=arm64` |
| Chart scheduling | Values pin arm64 through `nodeSelector`, an arm64 NodePool, or matching affinity and tolerations |
| Capacity | An arm64 NodePool exists in that cluster |

## Why

- **Price performance**: AWS positions Graviton as the best price performance option on EC2, and for most interpreted, JVM, and Go workloads the move needs no code change.
- **No big bang**: multi-arch images make the scheduling switch a values change that can be rolled forward and back per environment, with no rebuild.
- **Fast, faithful builds**: native runners build arm64 at full speed and run the real toolchain, while emulated builds are several times slower and hide architecture-specific failures until runtime.
- **Commitments carry over**: [Compute Savings Plans](https://docs.aws.amazon.com/savingsplans/latest/userguide/plan-types.html) apply regardless of instance family, so existing commitments keep covering the new Graviton instances.

## Exceptions

- Images with no arm64 build (often vendor agents, sidecars, and DaemonSets) keep their pods on amd64 with an explicit `kubernetes.io/arch: amd64` constraint until the vendor ships one
- Components that download a binary at runtime (controller plugins, init scripts) must fetch the binary matching the node, not a hardcoded `amd64` URL
- EC2 Instance Savings Plans and Reserved Instances are tied to an instance family, so check them before moving the capacity they cover
- Mirroring a third-party image into a private registry with `docker pull`, `docker tag`, and `docker push` copies only the host's platform. Copy the whole index instead, for example with [`crane copy`](https://github.com/google/go-containerregistry/blob/main/cmd/crane/doc/crane_copy.md)

## Example

GitLab CI multi-arch build on per-architecture runners:

```yaml
.deploy-docker:
  extends: .docker-dind
  before_script:
    - !reference [.registry-login, script]

deploy-docker-amd64:
  extends: .deploy-docker
  variables:
    ARCH: amd64
  script:
    - docker build --tag $IMAGE:$TAG-$ARCH .
    - docker push $IMAGE:$TAG-$ARCH

deploy-docker-arm64:
  extends: deploy-docker-amd64
  tags:
    - arm64
  variables:
    ARCH: arm64

deploy-manifest:
  extends: .deploy-docker
  dependencies: []
  needs:
    - deploy-docker-amd64
    - deploy-docker-arm64
  script:
    - docker buildx imagetools create --tag $IMAGE:$TAG $IMAGE:$TAG-amd64 $IMAGE:$TAG-arm64
```

Pin the workload to arm64:

```yaml
spec:
  nodeSelector:
    kubernetes.io/arch: arm64
```

Verify the image and the placement:

```bash
docker buildx imagetools inspect $IMAGE:$TAG
kubectl get pods -l app=my-app -o wide
kubectl get nodes -L kubernetes.io/arch
```

## References

- [AWS Graviton Technical Guide](https://github.com/aws/aws-graviton-getting-started)
- [Graviton containers guide](https://github.com/aws/aws-graviton-getting-started/blob/main/containers.md)
- [Docker multi-platform builds](https://docs.docker.com/build/building/multi-platform/)
- [Savings Plans types](https://docs.aws.amazon.com/savingsplans/latest/userguide/plan-types.html)
