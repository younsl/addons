# AGENTS.md

Guidance for coding agents working in this directory. Only non-derivable constraints and traps live here. Usage is in `README.md`, targets in the `Makefile`, structure in `src/`.

## Layout Rules

- Data comes in through the traits in `src/source.rs` (`CatalogSource`, `AddressSource`, `MetricsSource`, `PodSource`, `Tunnel`). `aws/` and `k8s/` implement them, `Collector` and `DbClient` only see the traits, and tests use fakes. A new backend implements a trait. It never adds SDK calls to `collector.rs`, `db/`, or `ui/`.
- The UI reaches the outside world only through `ui::Backend` (`watch`, `db`), implemented in `app.rs`. Calls return channels immediately, so the event loop never awaits the network.
- `ui/nav.rs` turns keys into `Command`s without side effects. `ui/rows.rs` is the single source of rows for both rendering and selection, so the selected index always matches the screen.
- `config::resolve` is the only place settings are merged. A new setting is added to `Cli`, `Layer`, and `Settings` together, or it silently skips a precedence level.

## Traps

- **Read only by construction.** `db/command.rs` is the complete list of commands sent to a cache. Never add a free-form command path or a write command.
- **Never exec into application containers for database traffic.** Queries go through the `valcon-probe` ephemeral container (`k8s/probe.rs`). Ephemeral containers cannot be removed, so reuse a running probe, prefer pods that already have one, and never add one per query. The `/proc/net/tcp` scan is the only exec into application containers and only runs `cat`.
- **A reply is whole only when the `QUIT` reply arrived.** `DbClient::call` requires one more reply than commands. Fewer means the tunnel cut off and the next gateway is tried.
- **Colors only from `ui/theme.rs`.** Black, grays, and white from the xterm 256 grayscale ramp. `render::tests` render every screen and fail on any other color.
- **Table columns go through `ui/render/columns.rs`.** Plain `Constraint::Length` lists get squeezed by the solver and truncate headers.
- **Dependency logs stay at `warn`.** aws-config logs access key ids and role ARNs at INFO.
- **`/proc/net/tcp` words are host byte order.** Parsing assumes a little-endian kernel (x86_64, arm64).
- **CloudWatch query ids encode positions** (`n{node}k{kind}`) because ids must start with a lowercase letter and node ids may not.
- **The TUI requires a TTY.** crossterm panics without one, so `app::run` checks first.
- **`config.toml` is gitignored on purpose.** It holds real contexts, namespaces, and registry paths. Only `example-config.toml` with placeholder values is committed.
