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
| Feed | The signed database: MalwareBazaar SHA-256 hashes and YARA Forge core rules. See [Signature database](signature-database.md) |
| YARA-X | Every YARA rule from the feed and `WardSignatures`, compiled with [YARA-X](https://github.com/VirusTotal/yara-x) when `scan.yara` is on |

### Precedence

1. An allow indicator (`spec.indicators.allow`) clears the file. Nothing else is checked.
2. A block indicator (`spec.indicators.block`) is a detection.
3. Otherwise every source is matched and the most severe match wins. The source does not matter, a `Critical` feed hash beats a `High` `WardSignature` pattern.

## Behavior rules

Behavior rules answer whether a process acts like an attack, even when no signature knows the file. They run on every process event from the [eBPF probes](ebpf.md) or the `/proc` poller, and each detection carries the process storyline (ancestors up to the container entrypoint).

| Rule | Detects | Severity |
| --- | --- | --- |
| `reverse-shell` | A shell whose stdin or stdout is a network socket | Critical |
| `shell-network-connect` | A shell or interpreter opening an outbound TCP connection | High |
| `fileless-exec` | Execution from `memfd` or a deleted file | High |
| `exec-from-tmp` | Execution from `/tmp`, `/dev/shm`, or `/var/tmp` | High |
| `crypto-miner` | Known miner processes and mining pool arguments | Critical |
| `webserver-shell` | A shell spawned by a web server or application runtime | Medium |
| `pipe-to-shell` | `curl` or `wget` output piped into a shell | High |
| `recon-tool` | Network scanners and raw socket tools in a container | Medium |
| `sa-token-read` | Reads of the ServiceAccount token from a shell | Medium |
| `exec-blocked` | An exec the BPF LSM denied, reported with Event reason `ExecBlocked` | High |

Turn rules off with `spec.behavior.disabledRules`. `exec-blocked` is not a rule you can disable, it reports an action ward already took. Pods in `exclusions.namespaces` are never scanned or acted on. Executables in `exclusions.processes` (exact absolute paths) skip the behavior rules only, so a malicious binary named or placed like an excluded one is still caught by its signature.

## File integrity monitoring

File integrity monitoring answers whether a sensitive host file changed. The agent hashes every path in `spec.integrity.paths` (directories one level deep), keeps the baseline on the node so a restart does not reset it, and reports every modified, created, or deleted file each `spec.integrity.interval`.

The default paths are `/etc/passwd`, `/etc/shadow`, `/etc/group`, `/etc/sudoers`, `/etc/sudoers.d`, `/etc/ssh/sshd_config`, `/root/.ssh/authorized_keys`, `/etc/ld.so.preload`, `/etc/crontab`, `/etc/cron.d`, and `/etc/kubernetes/manifests`. Integrity detections are alerts in both modes.

## Detection output

Every detection is one JSON line on the agent's stdout with a top-level `detection` object holding the engine, rule, severity, process, file, workload, and actions taken:

```bash
kubectl -n ward-system logs ds/ward-agent | jq 'select(.detection)'
```

The agent also publishes an Event on the affected Pod (reason `ThreatDetected`, or `ExecBlocked`) and, with the manager on, posts the detection to the manager store.

## Conclusion

Signatures catch known files, behavior rules catch known techniques, and integrity monitoring catches tampering with the node itself. What happens after a detection is in [Response](response.md).
