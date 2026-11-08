# Response

What the agent does after a detection in each mode. Response runs on the node itself and does not wait for the operator or the manager.

## Modes

| Mode | Behavior |
| --- | --- |
| `Detect` (default) | Every detection is an alert: stdout JSON, an Event, Lease counters, and the manager store |
| `Protect` | Alerts plus the actions below for high and critical detections |

Start with `Detect`, review the detections for a while, add allow indicators for anything legitimate, then switch to `Protect`.

## Actions in Protect mode

| Detection | Actions |
| --- | --- |
| Malicious file, scheduled scan | Quarantine, then blocklist its inode |
| Malicious executable, on exec | Kill the process, quarantine, then blocklist its inode |
| Behavior rule, High or Critical | Kill the process |
| Behavior rule, Medium or Low | Alert only |
| File integrity change | Alert only |

The actions taken are listed in the detection's `actions` field. A failed action (for example a read-only root filesystem that prevents quarantine) is reported in `actionError`, and the other actions still run.

### Kill

The agent sends `SIGKILL` to the process. For critical behavior detections such as a reverse shell or a miner it also kills the descendants it tracked in the process storyline.

### Quarantine

The agent moves the file into its quarantine directory on the node (`/var/lib/ward/<namespace>-<name>/quarantine` on the host), created readable only by root and named by its SHA-256, then removes the original from the container or host filesystem.

Container files are reached through `/proc/<pid>/root`, where an absolute symlink inside the container resolves against the host root. So the agent never reuses the scanned path. It resolves the path once with `openat2` and `RESOLVE_IN_ROOT`, refusing every symlink, checks that the opened file is the inode the scan saw, copies from that file descriptor, and unlinks through the verified parent directory descriptor. A container that swaps a directory for a symlink after the scan gets a refused quarantine, never a host file deleted or copied out. The quarantine copy is created with `O_EXCL` and mode `0400`, so it is never readable by anyone else even for an instant.

On exec, the quarantine runs before the kill, while `/proc/<pid>/root` still resolves. A behavior rule that matches the same process does not kill it a second time.

### Exec blocking

The agent adds the file's kernel device and inode to the BPF LSM blocklist. Any later exec of that file, in any container on the node, is denied with `EPERM` before it runs and reported as `exec-blocked` with Event reason `ExecBlocked`. Exec blocking needs the `ebpf-lsm` event source, see [eBPF probes](ebpf.md#event-source-modes).

## What is never touched

- Pods in `exclusions.namespaces`
- Executables in `exclusions.processes`, by behavior rules only (a signature match still triggers the response)
- Files whose hash is in `spec.indicators.allow`
- The agent's own process tree

## Conclusion

`Detect` gives visibility with no risk to workloads, and `Protect` adds kill, quarantine, and prevention on the node within the same event that detected the threat.
