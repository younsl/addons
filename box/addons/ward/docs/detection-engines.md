# Detection engines

The three engines the agent runs on every node: signature scanning, behavior rules, and file integrity monitoring. The design behind the scan pipeline is in [Design: detection engine](designs/detection-engine.md).

## Signature scanning

Signature scanning answers whether a file is known malware. The agent scans:

- the executable of every new process (`scan.onExec`), read through `/proc/<pid>/exe` so deleted and in-memory executables are covered
- every container filesystem and every `scan.hostPaths` directory on the `scan.interval` schedule

Files above `scan.maxFileSizeMib` and paths under `exclusions.paths` are skipped. Unchanged files are not rescanned, the scan cache is keyed by device, inode, size, and modification time.

### Sources

| Source | Content |
| --- | --- |
| Built-in | EICAR test file, XMRig miner, PHP web shells. Stored as hex so the agent binary never contains the bytes it hunts |
| `WardSignature` | Hashes, literal strings, hex patterns, and YARA rules from your namespace. See [Configuration](configuration.md#wardsignature) |
| Feed | The signed database: [malicious-hash](https://github.com/romainmarcoux/malicious-hash) SHA-256 lists and the YARA Forge core rules that can match on Linux. See [Signature database](signature-database.md) |
| YARA-X | Every YARA rule from the feed and `WardSignatures`, compiled with [YARA-X](https://github.com/VirusTotal/yara-x) when `scan.yara` is on |

### Precedence

1. An allow indicator (`spec.indicators.allow`) clears the file. Nothing else is checked.
2. A block indicator (`spec.indicators.block`) is a detection.
3. Otherwise every source is matched and the most severe match wins. The source does not matter, a `Critical` feed hash beats a `High` `WardSignature` pattern.

## Behavior rules

Behavior rules answer whether a process acts like an attack, even when no signature knows the file. They run on every process event from the [eBPF probes](ebpf.md) or the `/proc` poller, and each detection carries the process storyline (ancestors up to the container entrypoint).

| Rule | Detects | Severity |
| --- | --- | --- |
| `reverse-shell` | A shell whose stdin or stdout is a network socket, or a reverse shell one-liner (`/dev/tcp` with an interactive shell, `nc -e`, `socat exec:`, a `mkfifo` relay, Python, Perl, PHP, Ruby, or Node socket code) | Critical |
| `shell-network-connect` | A shell or interpreter opening an outbound TCP connection | High |
| `fileless-exec` | Execution from `memfd` or a deleted file | High |
| `exec-from-tmp` | Execution from `/tmp`, `/dev/shm`, or `/var/tmp` | High |
| `crypto-miner` | Known miner processes and mining pool arguments | Critical |
| `webserver-shell` | A shell spawned by a web server or application runtime | Medium |
| `pipe-to-shell` | `curl` or `wget` output, or a `base64 -d` payload, piped into a shell | High |
| `recon-tool` | Network scanners and raw socket tools in a container | Medium |
| `sa-token-read` | Reads of the ServiceAccount token from a shell | Medium |
| `container-escape` | In a container: `nsenter` into host PID 1, or a write to a host kernel hook (cgroup `release_agent`, `notify_on_release`, `core_pattern`, `uevent_helper`, `sysrq-trigger`) | Critical |
| `kernel-module-load` | In a container: `insmod`, or `modprobe` of a `.ko` file | High |
| `credential-access` | A file reader on `/etc/shadow`, kubeconfigs, kubelet or control plane PKI, cloud or Docker credentials, or SSH keys, and requests to instance metadata credential endpoints (EC2, EKS Pod Identity, ECS, GCP, Azure) | High |
| `attack-tool` | Post-exploitation tools (`pspy`, `cdk`, `deepce`, `peirates`, `traitor`, `frpc`, ...) and enumeration scripts (`linpeas`, `LinEnum`, `linux-exploit-suggester`, ...) | High |
| `runtime-socket` | In a container: `ctr`, `crictl`, `nerdctl`, or a command naming the Docker, containerd, or CRI-O socket | Medium |
| `persistence` | `crontab` installs and writes to cron, systemd units, init scripts, shell profiles, `authorized_keys`, or `/etc/ld.so.preload` | Medium |
| `defense-evasion` | Clearing shell history, overwriting or erasing auth and audit logs, `setenforce 0`, `auditctl -D`, and stopping security services | Medium |
| `exec-blocked` | An exec the BPF LSM denied, reported with Event reason `ExecBlocked` | High |

Rules match on the executable, its parent, and the command line (the first 512 bytes with eBPF). A command that only a shell builtin runs, such as a redirect into `release_agent` or `history -c`, is matched on the shell that runs it, and a command that starts its own process is matched on that process, so one action reports once.

Some rules match what privileged system workloads do on purpose. Exclude their namespace or executable before turning on `Protect`:

| Rule | Legitimate source |
| --- | --- |
| `container-escape` | Node shells (`kubectl debug node`, node-shell plugins) and reboot daemons such as kured that `nsenter` into PID 1 |
| `kernel-module-load` | GPU driver containers that load their own modules |
| `runtime-socket` | CI runners that build images through a mounted runtime socket |

Turn rules off with `spec.behavior.disabledRules`. `exec-blocked` is not a rule you can disable, it reports an action ward already took. Pods in `exclusions.namespaces` are never scanned or acted on. Executables in `exclusions.processes` (exact absolute paths) skip the behavior rules only, so a malicious binary named or placed like an excluded one is still caught by its signature.

## File integrity monitoring

File integrity monitoring answers whether a sensitive host file changed, how, and who changed it, at the level of [Wazuh syscheck](https://documentation.wazuh.com/current/user-manual/capabilities/file-integrity/index.html). The agent keeps a baseline of every file under `spec.integrity.paths` on the node, so a restart does not reset it.

| Wazuh option | Ward | What it does |
| --- | --- | --- |
| `realtime` | `realtime` (on) | inotify watches every monitored directory and the parent of every monitored file through the host root, and a change is reported within a second. Changes that arrive within half a second are checked together, so an editor's write, rename, and chmod make one report. The periodic check every `interval` stays on as a safety net, for example after an inotify queue overflow |
| `whodata` | `whodata` (on) | BPF LSM hooks on `file_open` (for writing), `inode_create`, `inode_unlink`, `inode_rename`, `path_chmod`, and `path_chown` report the process behind each change of a watched inode. The detection carries that process (pid, comm, exe, cmdline, uid, storyline) and its Pod, and `changes.operation` says which operation it was (`write`, `create`, `delete`, `rename-from`, `rename-to`, `attrib`). Needs the `ebpf-lsm` event source |
| `report_changes` | `reportChanges` (on) | Text files up to 256 KiB are copied to the node (mode `0600`) and a modification carries a unified diff, cut at 8 KiB |
| `nodiff` | `noDiff` | Paths whose content never goes into a copy or a diff. Defaults to `/etc/shadow`, `/etc/gshadow`, and `/etc/security/opasswd`. A file holding a private key is never copied whatever the list says |
| `recursion_level` | `recursionLevel` (256) | Directory levels below a monitored directory |
| `file_limit` | `fileLimit` (100000) | Files the baseline holds at most |
| `check_*` attributes | always on | `changes.attributes` lists what differs: `size`, `permission`, `uid`, `gid`, `mtime`, `inode`, `sha256` |

- `changes.mode` is `realtime` or `scheduled`, as in Wazuh's `syscheck.mode`.
- The whodata hooks never deny anything. `file_open` checks the open mode first, so only opens for writing reach the map lookup.
- An editor that writes a temporary file and renames it over the original (`sed -i`, `vim`) is attributed through the rename into the watched directory.
- A content change is attributed to a writer or an entry change, a mode or owner change only to `path_chmod` or `path_chown`, and only to actors seen since the previous check, so an old writer is never blamed for a later `chmod`. The mode and owner hooks need `CONFIG_SECURITY_PATH`, which distribution kernels set.
- Without the BPF LSM (`ebpf` and `procfs` modes) changes are still detected in real time, only without the process behind them.

The default paths are `/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/sudoers`, `/etc/sudoers.d`, `/etc/ssh/sshd_config`, `/root/.ssh/authorized_keys`, `/etc/ld.so.preload`, `/etc/crontab`, `/etc/cron.d`, and `/etc/kubernetes/manifests`. Integrity detections are alerts in both modes.

## Rootcheck

Rootcheck looks for the marks a rootkit leaves instead of known files, after the anomaly checks [Wazuh rootcheck](https://github.com/wazuh/wazuh/tree/main/docs/ref/modules/rootcheck) runs since Wazuh 5.0 (which dropped its rootkit file and trojan signature lists as outdated). Every `spec.rootcheck.interval` (default `12h`) the agent runs:

| Rule | Detects | Severity |
| --- | --- | --- |
| `hidden-process` | A PID that answers `kill(pid, 0)` and leads its thread group but is missing from the `/proc` listing, rechecked after a second listing | High |
| `setuid-in-temp` | A setuid or setgid file under the host `/tmp`, `/var/tmp`, or `/dev/shm` | High |
| `dev-regular-file` | A regular file in the host `/dev`, where only device nodes belong | Medium |
| `dev-hidden-entry` | A dot-named entry in the host `/dev`, other than the ones a stock system ships (`.udev`, `.initramfs`, ...) | Medium |

- The `/dev` and temporary directory walks stay on one file system, so `/dev/shm`, `/dev/pts`, and other mounts under `/dev` are not read as `/dev` itself.
- A finding is reported once per agent start, not on every run.
- Rootcheck detections are alerts in both modes. A hidden process cannot be trusted to be what `/proc` says, so ward does not act on it.
- Wazuh's hidden port and promiscuous interface checks are left out: agents do not share the host network namespace, and Kubernetes bridges and CNI interfaces run promiscuous by design.

## Detection output

Every detection is one JSON line on the agent's stdout with a top-level `detection` object holding the engine, rule, severity, process, file, workload, and actions taken:

```bash
kubectl -n ward-system logs ds/ward-agent | jq 'select(.detection)'
```

The agent also publishes an Event on the affected Pod (reason `ThreatDetected`, or `ExecBlocked`) and, with the manager on, posts the detection to the manager store.

## Conclusion

Signatures catch known files, behavior rules catch known techniques, integrity monitoring catches tampering with the node itself, and rootcheck catches what a rootkit hides. What happens after a detection is in [Response](response.md).
