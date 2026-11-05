# Screens and keys

What each valcon screen shows and how to move between them, for anyone using the TUI.

## Navigation

`Enter` opens the selected row, `Esc` goes back. `:` switches the root resource like [k9s](https://github.com/derailed/k9s).

```
groups > group (nodes | clients | pods) > node > db > key
                       clients > pods of an app > pod
:pods  > pod
:events
```

## Screens

| Screen | Content |
|--------|---------|
| Groups | Every replication group: engine, node type, mode, nodes, status, items, connections, CPU, memory, connected pods |
| Group, nodes tab | Role, shard, AZ, IP, items, connections, CPU, memory per node, and sockets scanned pods hold to it. Sparklines of connections and items |
| Group, clients tab | Apps (namespace and owner) connected to the group with pod count, sockets, and when last seen |
| Group, pods tab | Connected pods with sockets per node |
| Node | `INFO` summary (version, role, memory, clients, ops/s, hit rate) and keyspace per database |
| Keys | `SCAN` page with type, TTL, and memory usage. `/` sets the `MATCH` pattern, `n` and `p` page |
| Value | Preview by type: first 4 KiB of a string, first 100 fields, members, or elements, first 20 stream entries |
| `:pods` | Every scanned pod with sockets per group, scan errors, and whether a probe runs in it |
| `:events` | Group status and role changes, node IP changes, pods connecting to and disconnecting from groups, lookup errors |

## Keys

| Key | Action |
|-----|--------|
| `Enter`, `l`, `Right` | Open |
| `Esc`, `h`, `Left`, `Backspace` | Clear the filter, then go back |
| `j`, `k`, arrows, `PgUp`, `PgDn`, `g`, `G` | Move |
| `/` | Filter rows (on keys: `MATCH` pattern) |
| `:` | `groups`, `pods`, `events`, `quit` |
| `tab` | Next group tab |
| `n`, `p` | Next and previous key page |
| `r` | Refresh now, or reload the current database query |

`q`, `Ctrl+C`, or SIGTERM quits.

## Layout

Columns that do not fit the terminal are dropped by priority instead of being truncated. The palette is black, gray, and white only.

As in k9s, a number whose displayed value rose or fell on the latest refresh shows `↑` or `↓` to its right for 3 seconds.

## Conclusion

Every screen hangs off the groups list or a `:` root resource, `Enter` and `Esc` walk the tree, and `/` filters whatever is on screen. Narrow terminals lose low-priority columns rather than truncated values.
