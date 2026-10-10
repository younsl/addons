**ij** (Infra Janitor) is an EC2 operations CLI: SSM connect with fuzzy search, port forwarding, instance start and stop, and AMI cleanup. See the [README](https://github.com/${REPOSITORY}/blob/main/box/tools/ij/README.md) for usage.

## Changes

${CHANGES}

## Install

Requires AWS CLI v2 and the [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html).

```bash
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/')
curl -fsSLO https://github.com/${REPOSITORY}/releases/download/${PROJECT_NAME}/${VERSION}/ij-${OS}-${ARCH}.tar.gz
tar -xzf ij-${OS}-${ARCH}.tar.gz
sudo install -m 0755 ij-${OS}-${ARCH} /usr/local/bin/ij
```

${CHECKSUMS_TABLE}
