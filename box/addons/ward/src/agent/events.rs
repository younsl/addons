//! Process events, from the eBPF ring buffer or the `/proc` poller.

use std::net::IpAddr;
#[cfg(any(target_os = "linux", test))]
use std::net::{Ipv4Addr, Ipv6Addr};

#[cfg(any(target_os = "linux", test))]
use ward_ebpf_common::{
    BlockEvent, ConnectEvent, EVENT_BLOCK, EVENT_CONNECT, EVENT_EXEC, EVENT_EXIT, ExecEvent,
    ExitEvent, kind_of,
};

#[cfg(any(target_os = "linux", test))]
use super::procfs::args_to_string;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exec {
    pub pid: i32,
    pub ppid: Option<i32>,
    pub uid: Option<u32>,
    pub comm: String,
    pub filename: String,
    pub args: Option<String>,
    pub cgroup_id: u64,
    /// Kernel `dev_t` and inode the LSM hook saw. Zero without the LSM.
    pub dev: u64,
    pub ino: u64,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessEvent {
    Exec(Exec),
    Exit {
        pid: i32,
    },
    Connect {
        pid: i32,
        uid: u32,
        comm: String,
        addr: IpAddr,
        port: u16,
    },
    Blocked {
        pid: i32,
        uid: u32,
        comm: String,
        dev: u64,
        ino: u64,
        denied: bool,
    },
}

impl ProcessEvent {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Exec(_) => "exec",
            Self::Exit { .. } => "exit",
            Self::Connect { .. } => "connect",
            Self::Blocked { .. } => "block",
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|c| *c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

#[cfg(any(target_os = "linux", test))]
fn pid(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

#[cfg(any(target_os = "linux", test))]
/// Parses one ring buffer record.
pub fn parse(b: &[u8]) -> Option<ProcessEvent> {
    match kind_of(b)? {
        EVENT_EXEC => {
            let e = ExecEvent::parse(b)?;
            let n = (e.args_len as usize).min(e.args.len());
            Some(ProcessEvent::Exec(Exec {
                pid: pid(e.pid),
                ppid: (e.ppid != 0).then(|| pid(e.ppid)),
                uid: Some(e.uid),
                comm: cstr(&e.comm),
                filename: cstr(&e.filename),
                args: (n > 0).then(|| args_to_string(&e.args[..n])),
                cgroup_id: e.cgroup_id,
                dev: e.dev,
                ino: e.ino,
            }))
        }
        EVENT_EXIT => Some(ProcessEvent::Exit {
            pid: pid(ExitEvent::parse(b)?.pid),
        }),
        EVENT_CONNECT => {
            let e = ConnectEvent::parse(b)?;
            let addr = if e.family == 2 {
                IpAddr::V4(Ipv4Addr::new(
                    e.daddr[0], e.daddr[1], e.daddr[2], e.daddr[3],
                ))
            } else {
                IpAddr::V6(Ipv6Addr::from(e.daddr))
            };
            Some(ProcessEvent::Connect {
                pid: pid(e.pid),
                uid: e.uid,
                comm: cstr(&e.comm),
                addr,
                port: e.dport,
            })
        }
        EVENT_BLOCK => {
            let e = BlockEvent::parse(b)?;
            Some(ProcessEvent::Blocked {
                pid: pid(e.pid),
                uid: e.uid,
                comm: cstr(&e.comm),
                dev: e.dev,
                ino: e.ino,
                denied: e.denied != 0,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ward_ebpf_common::layout as l;

    use super::*;

    fn put(b: &mut [u8], off: usize, v: &[u8]) {
        b[off..off + v.len()].copy_from_slice(v);
    }

    pub fn exec_record(pid: u32, ppid: u32, comm: &str, file: &str, args: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; l::EXEC_SIZE];
        put(&mut b, 0, &EVENT_EXEC.to_ne_bytes());
        put(&mut b, l::EXEC_PID, &pid.to_ne_bytes());
        put(&mut b, l::EXEC_PPID, &ppid.to_ne_bytes());
        put(&mut b, l::EXEC_UID, &0u32.to_ne_bytes());
        put(&mut b, l::EXEC_CGROUP, &9u64.to_ne_bytes());
        put(&mut b, l::EXEC_DEV, &5u64.to_ne_bytes());
        put(&mut b, l::EXEC_INO, &77u64.to_ne_bytes());
        put(
            &mut b,
            l::EXEC_ARGS_LEN,
            &u32::try_from(args.len()).unwrap().to_ne_bytes(),
        );
        put(&mut b, l::EXEC_COMM, comm.as_bytes());
        put(&mut b, l::EXEC_FILENAME, file.as_bytes());
        put(&mut b, l::EXEC_ARGS, args);
        b
    }

    #[test]
    fn parses_every_record() {
        let e = parse(&exec_record(10, 1, "bash", "/bin/bash", b"bash\0-i\0")).unwrap();
        let ProcessEvent::Exec(x) = &e else {
            panic!("{e:?}")
        };
        assert_eq!((x.pid, x.ppid, x.comm.as_str()), (10, Some(1), "bash"));
        assert_eq!(x.filename, "/bin/bash");
        assert_eq!(x.args.as_deref(), Some("bash -i"));
        assert_eq!((x.dev, x.ino, x.cgroup_id), (5, 77, 9));
        assert_eq!(e.kind(), "exec");
        let orphan = parse(&exec_record(10, 0, "x", "/x", b"")).unwrap();
        assert!(matches!(
            orphan,
            ProcessEvent::Exec(Exec {
                ppid: None,
                args: None,
                ..
            })
        ));

        let mut exit = vec![0u8; l::EXIT_SIZE];
        put(&mut exit, 0, &EVENT_EXIT.to_ne_bytes());
        put(&mut exit, l::EXIT_PID, &10u32.to_ne_bytes());
        assert_eq!(parse(&exit), Some(ProcessEvent::Exit { pid: 10 }));

        let mut c = vec![0u8; l::CONNECT_SIZE];
        put(&mut c, 0, &EVENT_CONNECT.to_ne_bytes());
        put(&mut c, l::CONNECT_PID, &10u32.to_ne_bytes());
        put(&mut c, l::CONNECT_FAMILY, &2u16.to_ne_bytes());
        put(&mut c, l::CONNECT_DPORT, &4444u16.to_ne_bytes());
        put(&mut c, l::CONNECT_DADDR, &[10, 0, 0, 1]);
        put(&mut c, l::CONNECT_COMM, b"bash");
        let ev = parse(&c).unwrap();
        assert_eq!(ev.kind(), "connect");
        assert!(
            matches!(ev, ProcessEvent::Connect { port: 4444, ref comm, addr, .. } if comm == "bash" && addr == IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
        put(&mut c, l::CONNECT_FAMILY, &10u16.to_ne_bytes());
        assert!(matches!(
            parse(&c),
            Some(ProcessEvent::Connect {
                addr: IpAddr::V6(_),
                ..
            })
        ));

        let mut bl = vec![0u8; l::BLOCK_SIZE];
        put(&mut bl, 0, &EVENT_BLOCK.to_ne_bytes());
        put(&mut bl, l::BLOCK_PID, &3u32.to_ne_bytes());
        put(&mut bl, l::BLOCK_DENIED, &1u32.to_ne_bytes());
        put(&mut bl, l::BLOCK_DEV, &8u64.to_ne_bytes());
        put(&mut bl, l::BLOCK_INO, &9u64.to_ne_bytes());
        put(&mut bl, l::BLOCK_COMM, b"evil");
        let ev = parse(&bl).unwrap();
        assert_eq!(ev.kind(), "block");
        assert!(matches!(
            ev,
            ProcessEvent::Blocked {
                denied: true,
                dev: 8,
                ino: 9,
                ..
            }
        ));
        assert_eq!(ProcessEvent::Exit { pid: 1 }.kind(), "exit");

        assert!(parse(&[]).is_none());
        assert!(parse(&99u32.to_ne_bytes()).is_none());
        assert!(parse(&EVENT_EXEC.to_ne_bytes()).is_none());
    }
}
