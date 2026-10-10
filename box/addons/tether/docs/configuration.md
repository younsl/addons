# Configuration

Every setting tether reads, for anyone editing config.toml or running the container by hand. tether takes two kinds of input: runtime settings fixed at startup, and a config file it rereads on every reconcile.

## Runtime settings

Each setting is a flag with an environment variable fallback. The container sets them through environment variables.

| Flag | Env | Default | Notes |
| --- | --- | --- | --- |
| --config-file | CONFIG_FILE | /etc/tether/config.toml | Path inside the container |
| --home | HOME | required | Absolute path. ~ in the config file expands to it |
| --reconcile-interval | RECONCILE_INTERVAL | 5m | Duration such as 30s, 5m, or 1h. At least 1s |
| --dry-run | DRY_RUN | false | Plan and report without changing any file |
| --port | PORT | 8080 | Console, API, health, and metrics |
| --log-level | LOG_LEVEL | info | trace, debug, info, warn, or error |
| --log-format | LOG_FORMAT | json | json or text |

- RUST_LOG overrides LOG_LEVEL with a full filter such as tether=debug.
- PUT /log-level changes the filter while tether runs, without a restart.
- An invalid interval or a relative home stops tether at startup with the reason.

## Config file

The config file declares which source links to which home target, and which package files to regenerate. The repository copy is [config.toml](../config.toml).

- It is reread on every reconcile, so an edit takes effect at the next check or on POST /reconcile.
- Unknown keys fail the load. A typo shows up in the console and in /status instead of being silently ignored.
- While the file cannot be loaded, tether changes nothing, /readyz answers 503 Service Unavailable, and the console shows the error.

### Paths

| Written as | Resolves to |
| --- | --- |
| ~ | HOME |
| ~/PATH | HOME/PATH |
| /PATH | /PATH as is |
| PATH (link source or package file only) | source_root/PATH |

A relative path is accepted only where the table says so. A relative target, root, or packages read path such as homebrew_prefix fails the load.

### Top level

| Key | Required | Meaning |
| --- | --- | --- |
| source_root | yes | Directory relative link sources and package files resolve against |
| backup_root | yes | Where a real file in the way of a link is moved, under a UTC timestamp directory |
| links | no | Array of links, see below |
| packages | no | Package file settings, see below. Omit it to turn the feature off |

### links

```toml
[[links]]
source = "zsh/.zshrc"
target = "~/.zshrc"

[[links]]
source = "claude/skills"
target = "~/.claude/skills"
per_entry = true
```

| Key | Required | Default | Meaning |
| --- | --- | --- | --- |
| source | yes | | File or directory the link points to |
| target | yes | | Absolute or ~ path where the link is created. Each target is declared once |
| per_entry | no | false | Link each subdirectory of source into target instead of source itself |

Use per_entry when other tools also write into the target directory. With it, ~/.claude/skills stays a real directory: tether adds one link per skill, and entries Claude Code creates there are left alone.

### packages

```toml
[packages]
brewfile = "package-managers/brew/Brewfile"
krewfile = "package-managers/krew/krewfile"
npm_prefix = "~/.local/share/mise/installs/node/24"
```

brewfile and krewfile name the files to write. Leave one out to skip it. The other keys say where installed packages are read from, and their defaults fit a standard macOS setup.

| Key | Default | Read for |
| --- | --- | --- |
| brewfile | none | Brewfile to write |
| krewfile | none | krewfile to write |
| homebrew_prefix | /opt/homebrew | Taps, formulae, and casks |
| homebrew_cache | ~/Library/Caches/Homebrew | Formula and cask descriptions |
| trust_file | ~/.config/homebrew/trust.json | trusted: entries |
| launch_agents | ~/Library/LaunchAgents | restart_service: entries |
| krew_root | ~/.krew | krew plugins |
| cargo_home | ~/.cargo | cargo install entries |
| go_bin | ~/go/bin | go install entries |
| npm_prefix | none | Global npm packages. Without it, no npm entries are written |

- A file is rewritten only when its package lines change, so the date in its header never churns on its own.
- mas and vscode entries are not generated.
- npm_prefix points at a Node install, not at npm. With mise, a major version link such as node/24 keeps working across patch upgrades.

## Container mounts

tether sees only what the container mounts, and links are absolute paths, so each host path is mounted at the same path inside.

| Host path | Mount | Needed for |
| --- | --- | --- |
| HOME | read write, same path | Links, backups, and package files |
| config.toml | read only, at CONFIG_FILE | The config file |
| /opt/homebrew | read only, same path | Brewfile. Without it, tether logs a warning once and leaves the Brewfile as it is |

On macOS the container runtime runs in a VM that shares only some host folders. Podman shares /Users, /private, and /var/folders by default, so /opt/homebrew has to be added when the machine is created. Recreating the machine deletes its images and containers.

```bash
podman machine stop
podman machine rm -f
podman machine init --volume /Users:/Users --volume /private:/private \
  --volume /var/folders:/var/folders --volume /opt/homebrew:/opt/homebrew
podman machine start
```

[bootstrap-dotfiles.sh](../scripts/bootstrap/bootstrap-dotfiles.sh) adds the /opt/homebrew mount on its own once the VM shares it.

## Load errors

These are the errors /status and the console show when the config file cannot be used.

| Cause | Message |
| --- | --- |
| File missing or unreadable | read PATH: REASON |
| Not valid TOML, a missing required key, or an unknown key | parse PATH: REASON |
| A relative target, root, or packages read path | FIELD must be absolute or start with ~/, got VALUE |
| The same target declared twice | target PATH is declared more than once |
