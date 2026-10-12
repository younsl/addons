# Images

The images this component publishes and the harbor-helm chart value each one overrides.

Every image is single-architecture `linux/arm64`, tagged v2.15.3. harbor-helm has no 2.15.3 release yet, so deploy with [harbor-helm 1.19.2](https://github.com/goharbor/harbor-helm/releases/tag/v1.19.2) (app version 2.15.2) and let `harbor-arm64 values` override every image tag. See [Usage](usage.md).

| Image | Chart value ([harbor-helm 1.19.2](https://github.com/goharbor/harbor-helm/releases/tag/v1.19.2)) |
| --- | --- |
| `ghcr.io/younsl/harbor/nginx-photon:v2.15.3` | `nginx.image` |
| `ghcr.io/younsl/harbor/harbor-portal:v2.15.3` | `portal.image` |
| `ghcr.io/younsl/harbor/harbor-core:v2.15.3` | `core.image` |
| `ghcr.io/younsl/harbor/harbor-jobservice:v2.15.3` | `jobservice.image` |
| `ghcr.io/younsl/harbor/registry-photon:v2.15.3` | `registry.registry.image` |
| `ghcr.io/younsl/harbor/harbor-registryctl:v2.15.3` | `registry.controller.image` |
| `ghcr.io/younsl/harbor/trivy-adapter-photon:v2.15.3` | `trivy.image` |
| `ghcr.io/younsl/harbor/harbor-db:v2.15.3` | `database.internal.image` |
| `ghcr.io/younsl/harbor/valkey-photon:v2.15.3` | `redis.internal.image` |
| `ghcr.io/younsl/harbor/harbor-exporter:v2.15.3` | `exporter.image` |

`prepare` and `harbor-log` are not built. Only the docker-compose installer uses them.

## Conclusion

Ten linux/arm64 images replace every image harbor-helm 1.19.2 deploys. Deploy with that chart and let harbor-arm64 values set the tags.
