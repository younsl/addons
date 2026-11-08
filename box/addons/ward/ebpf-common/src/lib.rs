//! Layouts shared by the eBPF programs and the agent. Every event starts with
//! a `u32` kind so the agent can dispatch a ring buffer record before parsing
//! it.

#![no_std]

use core::mem::offset_of;

pub const COMM_LEN: usize = 16;
pub const PATH_LEN: usize = 256;
pub const ARGS_LEN: usize = 512;
pub const ADDR_LEN: usize = 16;

pub const EVENT_EXEC: u32 = 1;
pub const EVENT_EXIT: u32 = 2;
pub const EVENT_CONNECT: u32 = 3;
pub const EVENT_BLOCK: u32 = 4;

/// Index of the enforce flag in the `CONFIG` array. Non-zero lets the LSM
/// program deny execution of blocklisted inodes.
pub const CONFIG_ENFORCE: u32 = 0;
/// Index of the agent's own thread group ID in the `CONFIG` array. Its events
/// are dropped in the kernel.
pub const CONFIG_SELF_TGID: u32 = 1;
pub const CONFIG_LEN: u32 = 4;

pub const BLOCKLIST_MAX: u32 = 65_536;
pub const PENDING_MAX: u32 = 16_384;
/// Offset global value meaning the field could not be resolved.
pub const UNKNOWN_OFFSET: u32 = u32::MAX;
pub const RINGBUF_BYTES: u32 = 1 << 20;

/// Map and program names the agent looks up in the object.
pub mod names {
    pub const EVENTS: &str = "EVENTS";
    pub const CONFIG: &str = "CONFIG";
    pub const BLOCKLIST: &str = "BLOCKLIST";
    pub const PENDING: &str = "PENDING";
    pub const PROG_EXEC: &str = "ward_exec";
    pub const PROG_EXIT: &str = "ward_exit";
    pub const PROG_CONNECT: &str = "ward_connect";
    pub const PROG_BPRM_CHECK: &str = "ward_bprm_check";
}

/// Offsets the agent resolves at load time (tracefs formats and kernel BTF)
/// and writes into the object's read-only globals. Zero means unknown, and
/// the program skips the field.
pub mod globals {
    pub const TP_EXEC_FILENAME: &str = "TP_EXEC_FILENAME";
    pub const TP_SOCK_NEWSTATE: &str = "TP_SOCK_NEWSTATE";
    pub const TP_SOCK_DPORT: &str = "TP_SOCK_DPORT";
    pub const TP_SOCK_FAMILY: &str = "TP_SOCK_FAMILY";
    pub const TP_SOCK_PROTOCOL: &str = "TP_SOCK_PROTOCOL";
    pub const TP_SOCK_DADDR: &str = "TP_SOCK_DADDR";
    pub const TP_SOCK_DADDR_V6: &str = "TP_SOCK_DADDR_V6";
    pub const TASK_REAL_PARENT: &str = "TASK_REAL_PARENT";
    pub const TASK_TGID: &str = "TASK_TGID";
    pub const TASK_MM: &str = "TASK_MM";
    pub const MM_ARG_START: &str = "MM_ARG_START";
    pub const MM_ARG_END: &str = "MM_ARG_END";
    pub const BPRM_FILE: &str = "BPRM_FILE";
    pub const FILE_INODE: &str = "FILE_INODE";
    pub const INODE_INO: &str = "INODE_INO";
    pub const INODE_SB: &str = "INODE_SB";
    pub const SB_DEV: &str = "SB_DEV";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExecEvent {
    pub kind: u32,
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub cgroup_id: u64,
    /// Kernel `dev_t` and inode of the executable, as the LSM hook saw it.
    /// Zero when the LSM program is not attached.
    pub dev: u64,
    pub ino: u64,
    pub args_len: u32,
    pub pad: u32,
    pub comm: [u8; COMM_LEN],
    pub filename: [u8; PATH_LEN],
    pub args: [u8; ARGS_LEN],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ExitEvent {
    pub kind: u32,
    pub pid: u32,
    pub cgroup_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ConnectEvent {
    pub kind: u32,
    pub pid: u32,
    pub uid: u32,
    pub family: u16,
    pub dport: u16,
    pub cgroup_id: u64,
    pub daddr: [u8; ADDR_LEN],
    pub comm: [u8; COMM_LEN],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockEvent {
    pub kind: u32,
    pub pid: u32,
    pub uid: u32,
    /// Non-zero when the exec was denied, zero when only observed.
    pub denied: u32,
    pub cgroup_id: u64,
    pub dev: u64,
    pub ino: u64,
    pub comm: [u8; COMM_LEN],
}

/// The `BLOCKLIST` key: the kernel `dev_t` of the superblock, then the inode
/// number, both native endian.
pub type InodeKey = [u8; 16];

#[must_use]
pub const fn inode_key(dev: u64, ino: u64) -> InodeKey {
    let mut k = [0u8; 16];
    let (d, i) = k.split_at_mut(8);
    d.copy_from_slice(&dev.to_ne_bytes());
    i.copy_from_slice(&ino.to_ne_bytes());
    k
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn arr_at<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    b.get(off..off + N)?.try_into().ok()
}

/// The kind of a ring buffer record.
#[must_use]
pub fn kind_of(b: &[u8]) -> Option<u32> {
    u32_at(b, 0)
}

impl ExecEvent {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            kind: u32_at(b, offset_of!(Self, kind))?,
            pid: u32_at(b, offset_of!(Self, pid))?,
            ppid: u32_at(b, offset_of!(Self, ppid))?,
            uid: u32_at(b, offset_of!(Self, uid))?,
            cgroup_id: u64_at(b, offset_of!(Self, cgroup_id))?,
            dev: u64_at(b, offset_of!(Self, dev))?,
            ino: u64_at(b, offset_of!(Self, ino))?,
            args_len: u32_at(b, offset_of!(Self, args_len))?,
            pad: 0,
            comm: arr_at(b, offset_of!(Self, comm))?,
            filename: arr_at(b, offset_of!(Self, filename))?,
            args: arr_at(b, offset_of!(Self, args))?,
        })
    }
}

impl ExitEvent {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            kind: u32_at(b, offset_of!(Self, kind))?,
            pid: u32_at(b, offset_of!(Self, pid))?,
            cgroup_id: u64_at(b, offset_of!(Self, cgroup_id))?,
        })
    }
}

impl ConnectEvent {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            kind: u32_at(b, offset_of!(Self, kind))?,
            pid: u32_at(b, offset_of!(Self, pid))?,
            uid: u32_at(b, offset_of!(Self, uid))?,
            family: u16_at(b, offset_of!(Self, family))?,
            dport: u16_at(b, offset_of!(Self, dport))?,
            cgroup_id: u64_at(b, offset_of!(Self, cgroup_id))?,
            daddr: arr_at(b, offset_of!(Self, daddr))?,
            comm: arr_at(b, offset_of!(Self, comm))?,
        })
    }
}

impl BlockEvent {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        Some(Self {
            kind: u32_at(b, offset_of!(Self, kind))?,
            pid: u32_at(b, offset_of!(Self, pid))?,
            uid: u32_at(b, offset_of!(Self, uid))?,
            denied: u32_at(b, offset_of!(Self, denied))?,
            cgroup_id: u64_at(b, offset_of!(Self, cgroup_id))?,
            dev: u64_at(b, offset_of!(Self, dev))?,
            ino: u64_at(b, offset_of!(Self, ino))?,
            comm: arr_at(b, offset_of!(Self, comm))?,
        })
    }
}

/// Field offsets, so tests on the agent side can build records.
pub mod layout {
    use core::mem::{offset_of, size_of};

    use super::{BlockEvent, ConnectEvent, ExecEvent, ExitEvent};

    pub const EXEC_SIZE: usize = size_of::<ExecEvent>();
    pub const EXEC_PID: usize = offset_of!(ExecEvent, pid);
    pub const EXEC_PPID: usize = offset_of!(ExecEvent, ppid);
    pub const EXEC_UID: usize = offset_of!(ExecEvent, uid);
    pub const EXEC_CGROUP: usize = offset_of!(ExecEvent, cgroup_id);
    pub const EXEC_DEV: usize = offset_of!(ExecEvent, dev);
    pub const EXEC_INO: usize = offset_of!(ExecEvent, ino);
    pub const EXEC_ARGS_LEN: usize = offset_of!(ExecEvent, args_len);
    pub const EXEC_COMM: usize = offset_of!(ExecEvent, comm);
    pub const EXEC_FILENAME: usize = offset_of!(ExecEvent, filename);
    pub const EXEC_ARGS: usize = offset_of!(ExecEvent, args);
    pub const EXIT_SIZE: usize = size_of::<ExitEvent>();
    pub const EXIT_PID: usize = offset_of!(ExitEvent, pid);
    pub const CONNECT_SIZE: usize = size_of::<ConnectEvent>();
    pub const CONNECT_PID: usize = offset_of!(ConnectEvent, pid);
    pub const CONNECT_FAMILY: usize = offset_of!(ConnectEvent, family);
    pub const CONNECT_DPORT: usize = offset_of!(ConnectEvent, dport);
    pub const CONNECT_DADDR: usize = offset_of!(ConnectEvent, daddr);
    pub const CONNECT_COMM: usize = offset_of!(ConnectEvent, comm);
    pub const BLOCK_SIZE: usize = size_of::<BlockEvent>();
    pub const BLOCK_PID: usize = offset_of!(BlockEvent, pid);
    pub const BLOCK_DENIED: usize = offset_of!(BlockEvent, denied);
    pub const BLOCK_DEV: usize = offset_of!(BlockEvent, dev);
    pub const BLOCK_INO: usize = offset_of!(BlockEvent, ino);
    pub const BLOCK_COMM: usize = offset_of!(BlockEvent, comm);
}
