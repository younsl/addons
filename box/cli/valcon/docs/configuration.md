# Configuration

Every valcon setting with its flag, environment variable, and default, for users setting up targets.

## Precedence

Settings resolve in this order, highest first: command-line flag or its env var, the selected target in the config file, `[defaults]` in the config file, built-in defaults. The config file is `$XDG_CONFIG_HOME/valcon/config.toml` (default `~/.config/valcon/config.toml`) or `--config <path>`. See [example-config.toml](../example-config.toml). `config.toml` in the project directory is gitignored, so environment-specific values can live next to the source and be symlinked into place.

```bash
AWS_PROFILE=prod valcon              # default target from the config file
AWS_PROFILE=prod valcon staging      # named target
valcon -c prod -n orders,checkout    # flags only
valcon --list-targets                # print targets and exit
```

## Settings

| Key | Flag | Env | Default | Description |
|-----|------|-----|---------|-------------|
| `context` | `-c` | `VALCON_CONTEXT` | current-context | Kubeconfig context |
| `namespaces` | `-n a,b` | `VALCON_NAMESPACES` | context namespace | Namespaces whose pods are scanned |
| `all_namespaces` | `-A` | `VALCON_ALL_NAMESPACES` | `false` | Scan every namespace |
| `selector` | `-l` | `VALCON_SELECTOR` | none | Pod label selector |
| `port` | `--port` | `VALCON_PORT` | `6379` | Engine port |
| `aws_profile` | `-p` | `AWS_PROFILE` | SDK default | AWS profile |
| `aws_region` | `--aws-region` | `AWS_REGION` | profile region | AWS region |
| `interval` | `-i` | `VALCON_INTERVAL` | `15` | Seconds between refreshes |
| `parallel` | `-P` | `VALCON_PARALLEL` | `10` | Concurrent pod execs |
| `exec_timeout` | `--exec-timeout` | `VALCON_EXEC_TIMEOUT` | `10` | Per-exec timeout in seconds |
| `probe_image` | `--probe-image` | `VALCON_PROBE_IMAGE` | `valkey/valkey:9.1.3-alpine` | Ephemeral probe image, needs `nc` |
| `probe_timeout` | `--probe-timeout` | `VALCON_PROBE_TIMEOUT` | `60` | Seconds to wait for a new probe to start |

## Logging

Logs are written only with `--log-file` (the TUI owns the terminal). `--log-level`, `--log-format json|text`, and `RUST_LOG` control them.

## Conclusion

Put shared values under `[defaults]`, per-environment values under named targets, and override either with a flag or env var for one run. Logging stays off unless `--log-file` is set.
