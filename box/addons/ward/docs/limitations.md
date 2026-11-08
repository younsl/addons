# Limitations

Known gaps, so you can decide what ward covers and what needs another control.

- **Linux nodes only**: x86_64 and arm64. Windows and macOS nodes are not covered.
- **Exec blocking keys on the inode**: The BPF LSM denies the exact file that was found malicious, not a hash computed at exec time. A copy of that file under another inode is caught by the on-exec scan and killed, not denied up front.
- **eBPF needs the initial user namespace**: Rootless container runtimes and user-namespaced nodes (rootless Podman, kind on rootless Podman, `hostUsers: false`) refuse `bpf()` even to privileged containers. The agent falls back to `procfs` there and `status.eventSources` shows it.
- **Blocking needs the BPF LSM**: Nodes without `bpf` in the active LSM list run in `ebpf` or `procfs` mode and cannot block execution.
- **Scripts are scanned by path**: For an interpreter run such as `sh /tmp/x.sh`, the agent scans the interpreter and the script named in the exec. A script fed on stdin or through `-c` is only seen by the behavior rules.
- **Polling misses short-lived processes**: In `procfs` mode a process that exits within the poll interval is never seen by behavior rules.
- **No rollback of changes**: ward kills and quarantines, but does not undo file changes a process already made.
- **One Ward per cluster**: A second `Ward` is reported in status and deploys nothing.
- **One manager replica**: Detections posted while the manager restarts wait in the agent's bounded buffer, and the oldest are dropped if it fills. Logs and Events are not affected.
- **Plain Helm first install**: With `ward.create=true`, the first `helm install` fails because the CRDs and the `Ward` are in the same release. Argo CD is not affected. See [Installation](installation.md).
- **Feed scope**: The public feed covers MalwareBazaar hashes and the YARA Forge core rules. It is not a commercial threat intelligence feed. Add your own content with `WardSignature`.
- **Known-good coverage**: Feed pruning removes only what the build corpus (common base and runtime images) and the repository allowlist hold. A benign file outside them that a feed source lists is still detected. Allowlist it with `spec.indicators.allow`. Corpus images are pulled by tag, so a compromised upstream image can suppress at most `--max-pruned` entries per build, and each one shows in the workflow run summary.
- **Not a replacement for image scanning or admission control**: ward protects running workloads. Scan images before they run and restrict what may run.

## Conclusion

ward is a runtime layer. Pair it with image scanning and admission policies for defense in depth.
