# scripts

Repository maintenance scripts. Not part of any released artifact.

## clean-build-artifacts.sh

Reclaims disk space by removing Go and Rust build output. Rust `target/` directories dominate this repository: a full local build of all 15 crates reaches roughly 200 GB, since every crate keeps its own independent `target/` with both debug and release profiles.

Dry run is the default. Nothing is deleted without `--yes`.

```bash
./scripts/clean-build-artifacts.sh                     # report sizes and total, delete nothing
./scripts/clean-build-artifacts.sh --yes               # delete
./scripts/clean-build-artifacts.sh --yes --node        # also delete node_modules and dist
./scripts/clean-build-artifacts.sh --yes --local-only  # repository only, keep global caches
```

### What each flag removes

| Scope | Flag | Targets |
| --- | --- | --- |
| Repository | default | Every `target/` whose parent holds a `Cargo.toml`, plus any `.zig-cache` |
| Global | default | `~/Library/Caches/go-build`, `~/.cache/go-build`, `~/.cache/zig`, `GOMODCACHE`, `~/.cargo/registry/{cache,src}`, `~/.cargo/git/checkouts` |
| Repository | `--node` | Every `node_modules` and `dist` under the repository |
| Global | `--local-only` | Suppressed, leaving only the repository paths |

Sources, lockfiles, and `~/go/bin` are never touched.

### Recovery cost

Everything removed is regenerable, but not for free.

- The next build of each Rust crate is a cold rebuild with no incremental cache.
- Cargo re-downloads crate sources and git checkouts from the network.
- Go re-downloads modules on the next `go build`.
- After `--node`, Backstage needs `yarn install` before it builds again.

### Notes

A directory named `target` is only matched when a `Cargo.toml` sits next to it, so an unrelated directory of that name is left alone.

The Go module cache is stored read-only at mode 0444, and `rm -rf` fails on it with `Permission denied`. The script calls `go clean -modcache` instead, falling back to `chmod -R u+w` when `go` is not on `PATH`.

The `du` totals are measured before deletion, so the reported figure is what the paths held rather than what the filesystem later reports. On APFS the two differ, because clones and snapshots share blocks.

Both before and after, the script prints `df` for `/System/Volumes/Data`. On macOS, `df /` reports the read-only system volume and always looks nearly full, which is not the number worth watching.
