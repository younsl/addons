# How it works

How `harbor-arm64 build` turns the unmodified upstream source into verified arm64 images.

`harbor-arm64 build` clones `goharbor/harbor` at the tag in [VERSION](../VERSION) and runs the upstream Makefile on a native arm64 Docker host. The Makefile passes no `--platform`, so every compile container and image build runs on the host architecture. Emulation is not an option, the host itself must be arm64.

## Deviations from upstream

Only three things deviate from an upstream build:

| Deviation | Why |
| --- | --- |
| Build `goharbor/photon:5.0-legacy` locally from `make/photon/common/Dockerfile` | The published tag is amd64 only, so every `Dockerfile.base` would pull amd64 ([goharbor/harbor#23748](https://github.com/goharbor/harbor/issues/23748)). Base images are then built with `BUILD_BASE=true PULL_BASE_FROM_DOCKERHUB=false`. |
| `ENV GOARCH=amd64` becomes `arm64` in `make/photon/exporter/Dockerfile` | The exporter build stage pins the architecture. |
| Trivy asset `_Linux-64bit.tar.gz` becomes `_Linux-ARM64.tar.gz` in `Makefile` | The download URL names the x86_64 release asset. |

Each patch fails the build if its target text is missing, so an upstream change surfaces instead of silently producing amd64 artifacts.

## Verification

Before anything is pushed, the build checks that every image reports `arm64` and reads the ELF header of each compiled or downloaded binary (core, jobservice, registryctl, registry, trivy, scanner-trivy, exporter) to confirm it targets aarch64. Image metadata alone would pass an amd64 binary copied into an arm64 base.

## Conclusion

The build is upstream Harbor on a native arm64 host with three guarded patches. Every image and binary is checked for arm64 before push.
