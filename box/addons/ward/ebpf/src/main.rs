//! ward kernel programs.
//!
//! - `ward_exec`, `ward_exit`: process lifecycle from `sched` tracepoints
//! - `ward_connect`: outbound TCP from `sock:inet_sock_set_state`
//! - `ward_bprm_check`: BPF LSM hook that records the executable's inode and
//!   denies blocklisted inodes when enforcement is on
//!
//! Struct offsets come from the running kernel's BTF and tracefs formats,
//! written by the agent into the globals below before load.

#![no_std]
#![no_main]

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut, read_volatile};

use aya_ebpf::EbpfContext as _;
use aya_ebpf::helpers::generated::{
    bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid,
    bpf_get_current_task, bpf_get_current_uid_gid, bpf_probe_read_kernel,
    bpf_probe_read_kernel_str, bpf_probe_read_user,
};
use aya_ebpf::macros::{lsm, map, tracepoint};
use aya_ebpf::maps::{Array, HashMap, LruHashMap, RingBuf};
use aya_ebpf::programs::{LsmContext, TracePointContext};
use ward_ebpf_common::{
    ARGS_LEN, BLOCKLIST_MAX, BlockEvent, COMM_LEN, CONFIG_ENFORCE, CONFIG_LEN, CONFIG_SELF_TGID,
    ConnectEvent, EVENT_BLOCK, EVENT_CONNECT, EVENT_EXEC, EVENT_EXIT, ExecEvent, ExitEvent,
    InodeKey, PATH_LEN, PENDING_MAX, RINGBUF_BYTES, UNKNOWN_OFFSET, inode_key,
};

#[unsafe(link_section = "license")]
#[unsafe(no_mangle)]
static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(RINGBUF_BYTES, 0);
#[map]
static CONFIG: Array<u64> = Array::with_max_entries(CONFIG_LEN, 0);
#[map]
static BLOCKLIST: HashMap<InodeKey, u32> = HashMap::with_max_entries(BLOCKLIST_MAX, 0);
#[map]
static PENDING: LruHashMap<u32, InodeKey> = LruHashMap::with_max_entries(PENDING_MAX, 0);

#[unsafe(no_mangle)]
static TP_EXEC_FILENAME: u32 = 8;
#[unsafe(no_mangle)]
static TP_SOCK_NEWSTATE: u32 = 20;
#[unsafe(no_mangle)]
static TP_SOCK_DPORT: u32 = 26;
#[unsafe(no_mangle)]
static TP_SOCK_FAMILY: u32 = 28;
#[unsafe(no_mangle)]
static TP_SOCK_PROTOCOL: u32 = 30;
#[unsafe(no_mangle)]
static TP_SOCK_DADDR: u32 = 36;
#[unsafe(no_mangle)]
static TP_SOCK_DADDR_V6: u32 = 56;
#[unsafe(no_mangle)]
static TASK_REAL_PARENT: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static TASK_TGID: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static TASK_MM: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static MM_ARG_START: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static MM_ARG_END: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static BPRM_FILE: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static FILE_INODE: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static INODE_INO: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static INODE_SB: u32 = UNKNOWN_OFFSET;
#[unsafe(no_mangle)]
static SB_DEV: u32 = UNKNOWN_OFFSET;

const TCP_SYN_SENT: i32 = 2;
const IPPROTO_TCP: u16 = 6;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
const EPERM: i32 = 1;

#[inline(always)]
fn global(v: &'static u32) -> u32 {
    // SAFETY: a read of a static the loader may have rewritten in .rodata.
    unsafe { read_volatile(v) }
}

#[inline(always)]
fn config(index: u32) -> u64 {
    CONFIG.get(index).copied().unwrap_or(0)
}

#[inline(always)]
fn current_tgid() -> u32 {
    // SAFETY: helper without arguments.
    (unsafe { bpf_get_current_pid_tgid() } >> 32) as u32
}

#[inline(always)]
fn is_self(tgid: u32) -> bool {
    tgid != 0 && config(CONFIG_SELF_TGID) == u64::from(tgid)
}

/// Reads a `T` at `base + off` from kernel memory.
#[inline(always)]
unsafe fn read_kernel<T: Copy + Default>(base: u64, off: u32) -> Option<T> {
    if base == 0 || off == UNKNOWN_OFFSET {
        return None;
    }
    let mut v = T::default();
    // SAFETY: probe read copies at most size_of::<T>() bytes into `v` and
    // fails cleanly on a bad address.
    let r = unsafe {
        bpf_probe_read_kernel(
            addr_of_mut!(v).cast::<c_void>(),
            size_of::<T>() as u32,
            (base + u64::from(off)) as *const c_void,
        )
    };
    (r == 0).then_some(v)
}

#[inline(always)]
unsafe fn parent_tgid(task: u64) -> u32 {
    // SAFETY: probe reads of the current task.
    unsafe {
        read_kernel::<u64>(task, global(&TASK_REAL_PARENT))
            .and_then(|parent| read_kernel::<i32>(parent, global(&TASK_TGID)))
            .map_or(0, i32::cast_unsigned)
    }
}

#[tracepoint]
pub fn ward_exec(ctx: TracePointContext) -> u32 {
    // SAFETY: tracepoint context of sched_process_exec.
    unsafe { try_exec(&ctx) };
    0
}

unsafe fn try_exec(ctx: &TracePointContext) {
    let tgid = current_tgid();
    if is_self(tgid) {
        return;
    }
    let Some(mut entry) = EVENTS.reserve::<ExecEvent>(0) else {
        return;
    };
    let ev = entry.as_mut_ptr();
    // SAFETY: `ev` points at a reserved ring buffer record of ExecEvent size.
    unsafe {
        (*ev).kind = EVENT_EXEC;
        (*ev).pid = tgid;
        (*ev).uid = bpf_get_current_uid_gid() as u32;
        (*ev).cgroup_id = bpf_get_current_cgroup_id();
        (*ev).pad = 0;
        (*ev).dev = 0;
        (*ev).ino = 0;
        (*ev).args_len = 0;
        bpf_get_current_comm(addr_of_mut!((*ev).comm).cast(), COMM_LEN as u32);

        let filename = addr_of_mut!((*ev).filename).cast::<u8>();
        *filename = 0;
        let loc_off = global(&TP_EXEC_FILENAME);
        if loc_off != UNKNOWN_OFFSET
            && let Ok(loc) = ctx.read_at::<u32>(loc_off as usize)
        {
            let src = ctx.as_ptr().cast::<u8>().add((loc & 0xFFFF) as usize);
            bpf_probe_read_kernel_str(filename.cast(), PATH_LEN as u32, src.cast());
        }

        if let Some(key) = PENDING.get(&tgid) {
            let k = *key;
            let dev = u64::from_ne_bytes([k[0], k[1], k[2], k[3], k[4], k[5], k[6], k[7]]);
            let ino = u64::from_ne_bytes([k[8], k[9], k[10], k[11], k[12], k[13], k[14], k[15]]);
            (*ev).dev = dev;
            (*ev).ino = ino;
            let _ = PENDING.remove(&tgid);
        }

        let task = bpf_get_current_task();
        (*ev).ppid = parent_tgid(task);
        if let Some(mm) = read_kernel::<u64>(task, global(&TASK_MM))
            && let Some(start) = read_kernel::<u64>(mm, global(&MM_ARG_START))
            && let Some(end) = read_kernel::<u64>(mm, global(&MM_ARG_END))
            && end > start
        {
            let mut n = end - start;
            if n > (ARGS_LEN - 1) as u64 {
                n = (ARGS_LEN - 1) as u64;
            }
            let n = (n as u32) & (ARGS_LEN as u32 - 1);
            if n > 0
                && bpf_probe_read_user(addr_of_mut!((*ev).args).cast(), n, start as *const c_void)
                    == 0
            {
                (*ev).args_len = n;
            }
        }
    }
    entry.submit(0);
}

#[tracepoint]
pub fn ward_exit(_ctx: TracePointContext) -> u32 {
    // SAFETY: helper without arguments.
    let pid_tgid = unsafe { bpf_get_current_pid_tgid() };
    let pid = pid_tgid as u32;
    let tgid = (pid_tgid >> 32) as u32;
    if pid != tgid || is_self(tgid) {
        return 0;
    }
    if let Some(mut entry) = EVENTS.reserve::<ExitEvent>(0) {
        let ev = entry.as_mut_ptr();
        // SAFETY: `ev` points at a reserved record of ExitEvent size.
        unsafe {
            (*ev).kind = EVENT_EXIT;
            (*ev).pid = tgid;
            (*ev).cgroup_id = bpf_get_current_cgroup_id();
        }
        entry.submit(0);
    }
    0
}

#[tracepoint]
pub fn ward_connect(ctx: TracePointContext) -> u32 {
    // SAFETY: tracepoint context of sock:inet_sock_set_state.
    unsafe { try_connect(&ctx) };
    0
}

unsafe fn try_connect(ctx: &TracePointContext) {
    // SAFETY: reads within the tracepoint record at offsets from its format.
    unsafe {
        let Ok(newstate) = ctx.read_at::<i32>(global(&TP_SOCK_NEWSTATE) as usize) else {
            return;
        };
        if newstate != TCP_SYN_SENT {
            return;
        }
        let Ok(protocol) = ctx.read_at::<u16>(global(&TP_SOCK_PROTOCOL) as usize) else {
            return;
        };
        if protocol != IPPROTO_TCP {
            return;
        }
        let Ok(family) = ctx.read_at::<u16>(global(&TP_SOCK_FAMILY) as usize) else {
            return;
        };
        let tgid = current_tgid();
        if is_self(tgid) || (family != AF_INET && family != AF_INET6) {
            return;
        }
        let Some(mut entry) = EVENTS.reserve::<ConnectEvent>(0) else {
            return;
        };
        let ev = entry.as_mut_ptr();
        (*ev).kind = EVENT_CONNECT;
        (*ev).pid = tgid;
        (*ev).uid = bpf_get_current_uid_gid() as u32;
        (*ev).family = family;
        (*ev).dport = ctx
            .read_at::<u16>(global(&TP_SOCK_DPORT) as usize)
            .unwrap_or(0);
        (*ev).cgroup_id = bpf_get_current_cgroup_id();
        bpf_get_current_comm(addr_of_mut!((*ev).comm).cast(), COMM_LEN as u32);
        let daddr = addr_of_mut!((*ev).daddr);
        *daddr = [0; 16];
        if family == AF_INET {
            if let Ok(v4) = ctx.read_at::<[u8; 4]>(global(&TP_SOCK_DADDR) as usize) {
                *daddr.cast::<[u8; 4]>() = v4;
            }
        } else if let Ok(v6) = ctx.read_at::<[u8; 16]>(global(&TP_SOCK_DADDR_V6) as usize) {
            *daddr = v6;
        }
        entry.submit(0);
    }
}

#[lsm(hook = "bprm_check_security")]
pub fn ward_bprm_check(ctx: LsmContext) -> i32 {
    // SAFETY: LSM context of bprm_check_security(struct linux_binprm *).
    unsafe { try_bprm_check(&ctx) }
}

unsafe fn try_bprm_check(ctx: &LsmContext) -> i32 {
    let retval: i32 = ctx.arg(1);
    if retval != 0 {
        return retval;
    }
    let bprm: u64 = ctx.arg(0);
    // SAFETY: probe reads along bprm->file->f_inode->{i_ino, i_sb->s_dev}.
    let key = unsafe {
        let Some(file) = read_kernel::<u64>(bprm, global(&BPRM_FILE)) else {
            return 0;
        };
        let Some(inode) = read_kernel::<u64>(file, global(&FILE_INODE)) else {
            return 0;
        };
        let Some(ino) = read_kernel::<u64>(inode, global(&INODE_INO)) else {
            return 0;
        };
        let Some(sb) = read_kernel::<u64>(inode, global(&INODE_SB)) else {
            return 0;
        };
        let Some(dev) = read_kernel::<u32>(sb, global(&SB_DEV)) else {
            return 0;
        };
        inode_key(u64::from(dev), ino)
    };
    let tgid = current_tgid();
    let _ = PENDING.insert(&tgid, &key, 0);
    // SAFETY: the value is only checked for presence.
    if unsafe { BLOCKLIST.get(&key) }.is_none() {
        return 0;
    }
    let enforce = config(CONFIG_ENFORCE) != 0;
    if let Some(mut entry) = EVENTS.reserve::<BlockEvent>(0) {
        let ev = entry.as_mut_ptr();
        // SAFETY: `ev` points at a reserved record of BlockEvent size.
        unsafe {
            (*ev).kind = EVENT_BLOCK;
            (*ev).pid = tgid;
            (*ev).uid = bpf_get_current_uid_gid() as u32;
            (*ev).denied = u32::from(enforce);
            (*ev).cgroup_id = bpf_get_current_cgroup_id();
            let k = *addr_of!(key);
            (*ev).dev = u64::from_ne_bytes([k[0], k[1], k[2], k[3], k[4], k[5], k[6], k[7]]);
            (*ev).ino = u64::from_ne_bytes([k[8], k[9], k[10], k[11], k[12], k[13], k[14], k[15]]);
            bpf_get_current_comm(addr_of_mut!((*ev).comm).cast(), COMM_LEN as u32);
        }
        entry.submit(0);
    }
    if enforce { -EPERM } else { 0 }
}

#[cfg(not(test))]
#[panic_handler]
#[allow(clippy::missing_const_for_fn)]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
