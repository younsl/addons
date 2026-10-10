# external-ebs-autoresizer

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-external--ebs--autoresizer-black?style=flat-square&logo=docker&logoColor=white)](https://github.com/younsl/addons/pkgs/container/external-ebs-autoresizer)
[![Helm Chart](https://img.shields.io/badge/ghcr.io-charts%2Fexternal--ebs--autoresizer-black?style=flat-square&logo=helm&logoColor=white)](https://github.com/younsl/addons/pkgs/container/charts%2Fexternal-ebs-autoresizer)
[![Rust](https://img.shields.io/badge/rust-1.99.0-black?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=black)](https://github.com/younsl/addons/blob/main/LICENSE)

Automatically grows the [root filesystem][ebs-extend-fs] (ext2/3/4 or XFS) of **standalone EC2 instances** (EC2 outside the Kubernetes cluster, not EKS nodes) when disk usage crosses a threshold.

It works through [SSM Run Command][ssm-run-command] instead of SSH: it measures disk usage on the instance, grows the EBS volume with the EC2 [ModifyVolume][ec2-modifyvolume] API, then extends the filesystem in place. See [How it works](docs/how-it-works.md#ssm-execution-context).

It runs as a long-lived Deployment inside EKS and scans instances on an interval.
By default it considers every running instance in its account and region,
excluding EKS cluster nodes (managed node groups, self-managed nodes, and
Karpenter nodes) so it only ever touches standalone EC2. Set `tagFilters` to
narrow the candidate set further. For each instance over the threshold it [grows
the root EBS volume][ebs-modify] and [extends the filesystem][ebs-extend-fs] in
place. Every step is driven and logged by the addon itself rather than delegated
to an opaque SSM runbook, so each action has clear ownership and granular logs.
Built with Rust 1.99.0 and shipped as a statically linked musl binary
(cargo-zigbuild) on a `scratch` image for `linux/amd64` and `linux/arm64`.

## Features

- Grows the root EBS volume and extends the filesystem (ext2/3/4 or XFS) in place. See [How it works](docs/how-it-works.md)
- Targets standalone EC2 only, excluding EKS cluster nodes by default, narrowed further with `tagFilters`. See [Configuration](docs/configuration.md)
- Per-group resize policies that vary threshold and growth by tag or Name regex, with weighted precedence. See [Per-group resize policies](docs/configuration.md#per-group-resize-policies)
- Safety guards: max volume size, the AWS 6-hour modification cooldown, and dry run
- High availability through leader election across replicas. See [High availability](docs/how-it-works.md#high-availability)
- Protective auto-cordon, always on for EKS nodes, that keeps new Pods off a Node whose root disk crossed the threshold, marks it with a `ProtectiveCordon` Node condition, and uncordons it once usage is back under. See [Protective auto-cordon](docs/protective-auto-cordon.md)
- Optional gp3 throughput recommendations for in-cluster Nodes, piggybacked onto a size expansion. See [Node throughput recommendations](docs/throughput-recommendation.md)
- Always-on identification of unused PersistentVolumeClaims and PersistentVolumes. It never deletes one. See [Unused volume identification](docs/unused-volumes.md)
- Observability through [Prometheus metrics](docs/metrics.md), Kubernetes Events, [Alertmanager alerts](docs/alerting.md), and [Grafana annotations](docs/grafana-annotations.md)
- A [built-in CLI](docs/cli.md) to validate config and inspect policy reach without a running controller

## Quick start

Set up the IAM role and EKS Pod Identity association first, as described in [Installation](docs/installation.md). Then install the chart from its OCI registry:

```bash
helm install external-ebs-autoresizer \
  oci://ghcr.io/younsl/charts/external-ebs-autoresizer \
  --version x.y.z \
  --namespace kube-system \
  --set config.region=ap-northeast-2
```

Replace x.y.z with a released chart version. [Installation](docs/installation.md#install) shows how to list them.

## Documentation

Guides:

- [Installation](docs/installation.md): prerequisites, IAM policy, Helm install
- [Configuration](docs/configuration.md): every config key and per-group resize policies
- [How it works](docs/how-it-works.md): architecture, resize flow, SSM execution context, Events, high availability
- [Protective auto-cordon](docs/protective-auto-cordon.md)
- [Node throughput recommendations](docs/throughput-recommendation.md)
- [Unused volume identification](docs/unused-volumes.md)
- [Built-in CLI](docs/cli.md)
- [Metrics](docs/metrics.md)
- [Alerting](docs/alerting.md)
- [Grafana annotations](docs/grafana-annotations.md)
- [Development](docs/development.md): build, test, and package

Design documents:

- [Protective auto-cordon](docs/designs/protective-auto-cordon.md)
- [Node EBS throughput recommendation](docs/designs/ebs-throughput-recommendation.md)
- [Applying throughput recommendations on resize](docs/designs/throughput-apply-on-resize.md)
- [Unused volume identification](docs/designs/unused-volume-identification.md)

## License

This project is licensed under the Apache License 2.0. See the [LICENSE](../../../LICENSE) file for details.

[ebs-modify]: https://docs.aws.amazon.com/ebs/latest/userguide/requesting-ebs-volume-modifications.html
[ebs-extend-fs]: https://docs.aws.amazon.com/ebs/latest/userguide/recognize-expanded-volume-linux.html
[ec2-modifyvolume]: https://docs.aws.amazon.com/AWSEC2/latest/APIReference/API_ModifyVolume.html
[ssm-run-command]: https://docs.aws.amazon.com/systems-manager/latest/userguide/run-command.html
