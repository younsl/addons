# Usage

Running tether, its console, and its HTTP API.

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

- Add -v /opt/homebrew:/opt/homebrew:ro so tether can regenerate the Brewfile. The container runtime must share /opt/homebrew with its VM first, see [Container mounts](configuration.md#container-mounts).
- Start with -e DRY_RUN=true and read /api/status to see what the first reconcile would change.
- --user matches the owner of the mounted home.
- The port stays on loopback because the API is unauthenticated.

[bootstrap-dotfiles.sh](../scripts/bootstrap/bootstrap-dotfiles.sh) runs the same command with docker or podman.

## Console

Open http://127.0.0.1:8080 for the console: the result of the last reconcile, every link with its action, the package files, and a button that runs a reconcile now. Click a link or a package file to read its source with syntax highlighting. The viewer is read only, and it shows only files tracked in git, so local-only files such as work git settings and signing keys never appear.

http://127.0.0.1:8080/logs shows tether's own log: the last 1000 events, filtered by level and followed live, with the log level in effect.

## API

| Endpoint | Purpose |
| --- | --- |
| GET / | Console overview |
| GET /logs | Console log page |
| GET /api/status | Last reconcile report as JSON |
| GET /api/info | Version, config file, home, and interval as JSON |
| GET /api/tree?path=SOURCE | Tracked files under a link source or package file |
| GET /api/file?path=FILE | One tracked file with its content |
| GET /api/logs?after=SEQ | Log events newer than sequence number SEQ |
| POST /api/reconcile | Run a reconcile now (202 Accepted) |
| GET /api/log-level | The log filter in effect |
| PUT /api/log-level | Change the log filter at runtime, for example {"filter":"debug"} |
| GET /healthz | Liveness |
| GET /readyz | Ready once the link spec loads |
| GET /metrics | Prometheus metrics with the tether_ prefix |

```bash
curl -s -X POST 127.0.0.1:8080/api/reconcile
curl -s 127.0.0.1:8080/api/status | jq '.entries[] | select(.action != "in_sync")'
```

## Configuration

Runtime settings, config.toml keys, container mounts, and load errors are in [Configuration](configuration.md).
