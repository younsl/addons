# Usage

Running tether, its HTTP API, and its settings.

## Run

```bash
docker run -d --name tether \
  --restart unless-stopped \
  --user "$(id -u):$(id -g)" \
  -e HOME="$HOME" \
  -v "$HOME:$HOME" \
  -v "$PWD/config.toml:/etc/tether/config.toml:ro" \
  -p 127.0.0.1:8080:8080 \
  ghcr.io/younsl/tether:0.1.0
```

- Start with -e DRY_RUN=true and read /status to see what the first reconcile would change.
- --user matches the owner of the mounted home.
- The port stays on loopback because /reconcile is unauthenticated.

[bootstrap-dotfiles.sh](../scripts/bootstrap/bootstrap-dotfiles.sh) runs the same command with docker or podman.

## API

| Endpoint | Purpose |
| --- | --- |
| GET /status | Last reconcile report as JSON |
| POST /reconcile | Run a reconcile now (202 Accepted) |
| GET /healthz | Liveness |
| GET /readyz | Ready once the link spec loads |
| GET /metrics | Prometheus metrics with the tether_ prefix |

```bash
curl -s -X POST 127.0.0.1:8080/reconcile
curl -s 127.0.0.1:8080/status | jq '.entries[] | select(.action != "in_sync")'
```

## Settings

| Flag | Env | Default |
| --- | --- | --- |
| --config-file | CONFIG_FILE | /etc/tether/config.toml |
| --home | HOME | required |
| --reconcile-interval | RECONCILE_INTERVAL | 5m |
| --dry-run | DRY_RUN | false |
| --port | PORT | 8080 |
| --log-level | LOG_LEVEL | info |
| --log-format | LOG_FORMAT | json |

The startup logo goes to stderr, so stdout stays pure JSON logs.

## Link spec

```toml
source_root = "~/github/younsl/addons/box/addons/tether/configs"
backup_root = "~/.dotfiles-backup"

[[links]]
source = "zsh/.zshrc"
target = "~/.zshrc"

[[links]]
source = "claude/skills"
target = "~/.claude/skills"
per_entry = true
```

- ~/ expands to HOME. A relative source resolves against source_root.
- Every target is absolute after expansion and declared once.
- Unknown keys fail the load, so a typo shows up in /status instead of being ignored.
- The spec is reread on every reconcile, so edits take effect without a restart.
