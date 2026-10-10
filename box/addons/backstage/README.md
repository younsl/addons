# Backstage with GitLab Discovery

[![GHCR](https://img.shields.io/badge/GHCR-ghcr.io%2Fyounsl%2Fbackstage-black?style=flat-square&logo=github&logoColor=white)](https://ghcr.io/younsl/backstage)
[![Backstage](https://img.shields.io/badge/Backstage-1.55.3-black?style=flat-square&logo=backstage&logoColor=white)](https://github.com/backstage/backstage/releases/tag/v1.55.3)

Custom Backstage image with GitLab auto-discovery, [Keycloak](https://github.com/keycloak/keycloak) OIDC, and in-house plugins built on [Backstage UI](https://backstage.io/docs/getting-started/ui) (BUI). Optimized for the official [Backstage Helm chart](https://github.com/backstage/charts): just swap the image.

## Announcement

### Plugins moved to an internal platform (2026-10-03)

Most in-house plugins are retired. Operational tooling such as access audits, account requests, log extraction and capacity reservations moved to a dedicated internal platform, so this image now leans on Backstage far less. What stays is the portal core (catalog, API docs, TechDocs, templates, platforms, cost report). The [changelog](docs/changelog.md) lists the removed plugins.

## Quick Start

```bash
make init   # install deps
make dev    # frontend :3000, backend :7007
make build  # build container image
make run    # run container locally (requires .env)
```

Authentication is [Keycloak](https://github.com/keycloak/keycloak) OIDC only. Guest login is disabled.

## Documentation

- [Installation](docs/installation.md)
- [Local Development](docs/local-development.md)
- [Plugins](docs/plugins.md)
- [Helm Chart](docs/helm-chart.md)
- [Changelog](docs/changelog.md)

### Plugin Docs

- [Keycloak OIDC](docs/plugins/auth-backend-module-oidc-provider/overview.md)
- [GitLab Discovery](docs/plugins/catalog-backend-module-gitlab/discovery.md)
- [GitLab API Discovery](docs/plugins/catalog-backend-module-gitlab/api-discovery.md)
- [OpenCost](docs/plugins/opencost/overview.md)
- [OpenCost ERD](docs/plugins/opencost/erd.md)
