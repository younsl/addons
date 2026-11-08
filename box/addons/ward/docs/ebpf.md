# eBPF probes

The kernel programs the agent loads, what each one observes, how the agent falls back when a node cannot run them, and what nodes need. The full design is in [Design: eBPF detection](designs/ebpf-detection.md).

## Probes

The agent embeds one eBPF object written in Rust with [aya](https://github.com/aya-rs/aya), built for x86_64 and arm64.

| Program | Hook | Reports |
| --- | --- | --- |
| `ward_exec` | tracepoint `sched:sched_process_exec` | Every exec with pid, ppid, uid, cgroup, comm, filename, and up to 511 bytes of argv |
| `ward_exit` | tracepoint `sched:sched_process_exit` | Thread group exits, so the process table stays small |
| `ward_connect` | tracepoint `sock:inet_sock_set_state` | Outbound TCP connects (transition to `SYN_SENT`) with destination address and port |
| `ward_bprm_check` | BPF LSM `bprm_check_security` | The kernel inode of each executable, and a denial when that inode is blocklisted and enforcement is on |

Tracepoints see processes that live for milliseconds, which `/proc` polling misses. The argv read stops at the end of the argument area, so environment variables (and the secrets they may hold) never leave the kernel.

## Exec blocking

In `Protect` mode a malicious executable joins the in-kernel blocklist under the exact device and inode the LSM hook saw when it ran. The next exec of that file is denied with `EPERM` before the program starts, the agent reports it as `exec-blocked` with Event reason `ExecBlocked`. See [Response](response.md#exec-blocking).

## Event source modes

The agent picks the strongest source the node allows and reports it in its Lease and in the `Ward` status (`status.eventSources`).

| Mode | Needs | Exec blocking | Short-lived processes |
| --- | --- | --- | --- |
| `ebpf-lsm` | BTF, kernel 5.8+, BPF LSM | Yes | Seen |
| `ebpf` | BTF, kernel 5.8+ | No | Seen |
| `procfs` | Nothing | No | May be missed |

`spec.agent.eventSource: Auto` walks this chain from the top. `Ebpf` requires eBPF and reports an error instead of falling back. `Procfs` skips eBPF entirely.

## Portability

There is no per-kernel build. At load time the agent reads struct offsets from the running kernel's BTF (`/sys/kernel/btf/vmlinux`) and tracepoint field offsets from tracefs, and writes them into read-only globals of the object. A field the kernel does not have is skipped rather than read at a wrong offset.

## Node requirements

- Kernel 5.8 or later with BTF, for the ring buffer and offset resolution.
- For `ebpf-lsm`: `bpf` in `/sys/kernel/security/lsm`. Distributions that do not enable it by default need the `lsm=` boot parameter with `bpf` appended to the existing list.
- Capabilities `BPF`, `PERFMON`, `SYS_RESOURCE`, `SYS_PTRACE`, `MAC_ADMIN`, `KILL`, `DAC_READ_SEARCH`, `DAC_OVERRIDE`, and `FOWNER`, which the operator sets on the agent. The last two let quarantine remove files owned by other users. Set `spec.agent.privileged: true` where AppArmor, SELinux, or a seccomp profile still blocks `bpf()`, which also needs the chart value `operator.allowPrivilegedAgent: true`.
- tracefs mounted at `/sys/kernel/tracing` on the host, which the agent mounts read-only.

Check a node:

```bash
ls /sys/kernel/btf/vmlinux
cat /sys/kernel/security/lsm
```

## License

The eBPF programs in `ebpf/` are dual licensed MIT or GPL-2.0. The kernel grants GPL-only helpers such as `bpf_probe_read_kernel` and LSM attachment only to programs that declare a GPL-compatible license. The rest of ward is Apache-2.0.

## Building

The object builds on the pinned stable toolchain (1.99.1) with `RUSTC_BOOTSTRAP=1 -Z build-std=core` and the prebuilt [bpf-linker](https://github.com/aya-rs/bpf-linker), so no nightly toolchain is involved. Without the object the binary still builds and every agent runs in `procfs` mode.

```bash
make ebpf
```

## Conclusion

The tracepoints give the agent every process and connection in real time, and the LSM hook turns a detection into prevention. Nodes that cannot run them still get signature scanning, integrity monitoring, and polled behavior rules.
