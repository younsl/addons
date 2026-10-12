# harbor

[![GitHub Container Registry](https://img.shields.io/badge/ghcr.io-harbor%2F*-black?style=flat-square&logo=docker&logoColor=white)](https://github.com/younsl?tab=packages&repo_name=addons&q=harbor)
[![Harbor](https://img.shields.io/badge/harbor-v2.15.3-black?style=flat-square&logo=harbor&logoColor=white)](https://github.com/goharbor/harbor/releases/tag/v2.15.3)
[![Platform](https://img.shields.io/badge/platform-linux%2Farm64-black?style=flat-square&logo=linux&logoColor=white)](https://aws.amazon.com/ec2/graviton/)
[![Rust](https://img.shields.io/badge/rust-1.99.0-black?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![GitHub license](https://img.shields.io/github/license/younsl/addons?style=flat-square&color=black)](https://github.com/younsl/addons/blob/main/LICENSE)

<img src="docs/assets/harbor-logo.svg" alt="Harbor logo" width="320">

A bundle of custom-built [Harbor](https://goharbor.io/) v2.15.3 images for [AWS Graviton](https://aws.amazon.com/ec2/graviton/) (linux/arm64). Upstream publishes the v2.15.x images for amd64 only, so this component rebuilds every image the [harbor-helm](https://github.com/goharbor/harbor-helm) chart deploys from the unmodified upstream source, driven by harbor-arm64, a small Rust CLI.

## Announcement

This is a stopgap. [Harbor v2.16.0](https://github.com/goharbor/harbor/issues/23558) ships official linux/arm64 images, [planned](https://github.com/goharbor/harbor/issues/24016) for late October 2026. Once it is released, this component and every image under ghcr.io/younsl/harbor will be removed.

## Documentation

- [Images](docs/images.md): published images and the harbor-helm value each one overrides
- [Usage](docs/usage.md): deploy with harbor-helm on Graviton nodes
- [How it works](docs/how-it-works.md): build flow, upstream deviations, arm64 verification
- [Test results](docs/test-results.md): production verification on EKS
- [Building](docs/building.md): requirements, commands, and CLI flags
- [Release](docs/release.md): VERSION-driven release workflow
- [Development](docs/development.md): test, coverage, and lint targets

## License

Apache License 2.0. See [LICENSE](../../../LICENSE). Harbor itself is licensed under Apache License 2.0 by the Harbor authors. The Harbor logo comes from [cncf/artwork](https://github.com/cncf/artwork/tree/main/projects/harbor) and is a trademark of The Linux Foundation.
