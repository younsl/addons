//! Real-time file integrity monitoring with inotify, after Wazuh's
//! `realtime` option. Watches every monitored directory (down to the
//! recursion level) and the parent of every monitored file through the host
//! root, and reports the host paths that changed. The periodic check stays
//! on as a safety net for anything inotify misses, such as a queue overflow.

use std::collections::HashMap;
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use tokio::sync::mpsc;
use walkdir::WalkDir;

use super::integrity::Limits;

const MAX_WATCHES: usize = 8192;

fn mask() -> AddWatchFlags {
    AddWatchFlags::IN_MODIFY
        | AddWatchFlags::IN_ATTRIB
        | AddWatchFlags::IN_CLOSE_WRITE
        | AddWatchFlags::IN_CREATE
        | AddWatchFlags::IN_DELETE
        | AddWatchFlags::IN_MOVED_FROM
        | AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF
        | AddWatchFlags::IN_DONT_FOLLOW
        | AddWatchFlags::IN_ONLYDIR
}

/// Whether a change at `path` concerns a watched root.
fn relevant(roots: &[String], path: &str) -> bool {
    roots.iter().any(|r| {
        let r = r.trim_end_matches('/');
        path == r
            || path
                .strip_prefix(r)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

struct Watches {
    inotify: Inotify,
    host_root: PathBuf,
    /// Watch descriptor to the host path of its directory.
    dirs: HashMap<WatchDescriptor, String>,
}

impl Watches {
    fn add(&mut self, host_path: &str) {
        if self.dirs.len() >= MAX_WATCHES {
            return;
        }
        let real = self.host_root.join(host_path.trim_start_matches('/'));
        match self.inotify.add_watch(&real, mask()) {
            Ok(wd) => {
                self.dirs.insert(wd, host_path.to_string());
            }
            Err(e) => tracing::debug!(error = %e, path = host_path, "inotify watch"),
        }
    }

    /// Watches `dir` and its subdirectories down to `levels` more levels.
    fn add_tree(&mut self, dir: &str, levels: usize) {
        let real = self.host_root.join(dir.trim_start_matches('/'));
        let subdirs: Vec<String> = WalkDir::new(&real)
            .follow_links(false)
            .max_depth(levels)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_dir())
            .map(|e| {
                let rel = e.path().strip_prefix(&real).unwrap_or_else(|_| e.path());
                if rel.as_os_str().is_empty() {
                    dir.to_string()
                } else {
                    format!("{}/{}", dir.trim_end_matches('/'), rel.display())
                }
            })
            .collect();
        for d in subdirs {
            self.add(&d);
        }
    }
}

/// Watches the roots until `stop` is set, sending each batch of changed
/// host paths. Runs on its own thread because inotify is a blocking read.
pub fn watch(
    host_root: &Path,
    roots: &[String],
    limits: Limits,
    tx: &mpsc::Sender<Vec<String>>,
    stop: &Arc<AtomicBool>,
) -> nix::Result<()> {
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
    let mut w = Watches {
        inotify,
        host_root: host_root.to_path_buf(),
        dirs: HashMap::new(),
    };
    for r in roots {
        let real = host_root.join(r.trim_start_matches('/'));
        if real.is_dir() {
            w.add_tree(r, limits.recursion_level.saturating_add(1));
        } else if let Some(parent) = Path::new(r).parent() {
            w.add(&parent.display().to_string());
        }
    }
    tracing::info!(watches = w.dirs.len(), "real-time integrity monitoring on");
    while !stop.load(Ordering::Relaxed) {
        let ready = {
            let mut fds = [PollFd::new(w.inotify.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, PollTimeout::from(1000u16))?
        };
        if ready == 0 {
            continue;
        }
        let events = match w.inotify.read_events() {
            Ok(e) => e,
            Err(nix::errno::Errno::EAGAIN) => continue,
            Err(e) => return Err(e),
        };
        let mut changed = Vec::new();
        for ev in events {
            if ev.mask.contains(AddWatchFlags::IN_Q_OVERFLOW) {
                tracing::warn!("inotify queue overflowed, the next periodic check catches up");
                continue;
            }
            let Some(dir) = w.dirs.get(&ev.wd).cloned() else {
                continue;
            };
            let path = ev.name.as_ref().map_or_else(
                || dir.clone(),
                |n| format!("{}/{}", dir.trim_end_matches('/'), n.to_string_lossy()),
            );
            if ev.mask.contains(AddWatchFlags::IN_ISDIR)
                && ev
                    .mask
                    .intersects(AddWatchFlags::IN_CREATE | AddWatchFlags::IN_MOVED_TO)
                && relevant(roots, &path)
            {
                w.add_tree(&path, limits.recursion_level);
            }
            if ev.mask.contains(AddWatchFlags::IN_IGNORED) {
                w.dirs.remove(&ev.wd);
            }
            if relevant(roots, &path) {
                changed.push(path);
            }
        }
        changed.sort();
        changed.dedup();
        if !changed.is_empty() && tx.blocking_send(changed).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn only_watched_paths_are_relevant() {
        let roots = vec!["/etc/passwd".to_string(), "/etc/cron.d/".into()];
        assert!(relevant(&roots, "/etc/passwd"));
        assert!(relevant(&roots, "/etc/cron.d/job"));
        assert!(relevant(&roots, "/etc/cron.d"));
        assert!(!relevant(&roots, "/etc/passwd-"));
        assert!(!relevant(&roots, "/etc/hosts"));
    }

    #[tokio::test]
    async fn reports_changes_as_they_happen() {
        let host = tempfile::tempdir().unwrap();
        let h = host.path().to_path_buf();
        std::fs::create_dir_all(h.join("etc/cron.d")).unwrap();
        std::fs::write(h.join("etc/passwd"), "root").unwrap();
        let roots = vec!["/etc/passwd".to_string(), "/etc/cron.d".into()];
        let (tx, mut rx) = mpsc::channel(16);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (h, roots, stop) = (h.clone(), roots.clone(), stop.clone());
            std::thread::spawn(move || {
                watch(
                    &h,
                    &roots,
                    Limits {
                        recursion_level: 4,
                        file_limit: 100,
                    },
                    &tx,
                    &stop,
                )
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(h.join("etc/hosts"), "ignored").unwrap();
        std::fs::write(h.join("etc/passwd"), "root\nevil").unwrap();
        std::fs::create_dir_all(h.join("etc/cron.d/new")).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(h.join("etc/cron.d/new/job"), "* * * * * x").unwrap();
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(seen.contains(&"/etc/passwd".to_string())
            && seen.contains(&"/etc/cron.d/new/job".to_string()))
        {
            let Ok(Some(batch)) = tokio::time::timeout_at(deadline, rx.recv()).await else {
                break;
            };
            seen.extend(batch);
        }
        stop.store(true, Ordering::Relaxed);
        thread.join().unwrap().unwrap();
        assert!(seen.contains(&"/etc/passwd".to_string()), "{seen:?}");
        assert!(
            seen.contains(&"/etc/cron.d/new/job".to_string()),
            "{seen:?}"
        );
        assert!(!seen.contains(&"/etc/hosts".to_string()), "{seen:?}");
    }
}
