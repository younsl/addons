//! PTY-based session handling with escape sequence detection.

use colored::Colorize;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::termios::{self, SetArg};
use pty_process::Size;
use pty_process::blocking::{Command as PtyCommand, Pty};
use signal_hook::consts::{SIGINT, SIGTSTP, SIGWINCH};
use std::io::{Read, Stdin, Write};
use std::os::fd::AsFd;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tracing::{debug, info};

mod escape;

/// Errors raised while running a PTY-backed session.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("Failed to open PTY")]
    Open(#[source] pty_process::Error),
    #[error("Failed to set raw mode")]
    RawMode(#[source] nix::Error),
    #[error("Failed to spawn aws ssm start-session")]
    Spawn(#[source] pty_process::Error),
    #[error(transparent)]
    Poll(#[from] nix::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, PtyError>;

/// Flags shared with the process-wide signal handlers.
struct SessionSignals {
    /// While false, `SIGINT` and `SIGTSTP` are ignored instead of running their default action.
    default_action: Arc<AtomicBool>,
    /// Set when `SIGWINCH` was received.
    winch: Arc<AtomicBool>,
}

static SIGNALS: OnceLock<SessionSignals> = OnceLock::new();

fn session_signals() -> &'static SessionSignals {
    SIGNALS.get_or_init(|| {
        let default_action = Arc::new(AtomicBool::new(true));
        let winch = Arc::new(AtomicBool::new(false));
        for signal in [SIGINT, SIGTSTP] {
            signal_hook::flag::register_conditional_default(signal, Arc::clone(&default_action))
                .ok();
        }
        signal_hook::flag::register(SIGWINCH, Arc::clone(&winch)).ok();
        SessionSignals {
            default_action,
            winch,
        }
    })
}

/// Connect to SSM session with PTY and escape sequence detection.
pub fn connect_with_pty(cmd: &Command) -> Result<()> {
    let (pty, pts) = pty_process::blocking::open().map_err(PtyError::Open)?;

    let stdin = std::io::stdin();
    copy_window_size(&stdin, &pty);
    let original_termios = termios::tcgetattr(&stdin).ok();

    if let Some(ref orig) = original_termios {
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).map_err(PtyError::RawMode)?;
    }

    let mut child = match to_pty_command(cmd).spawn(pts) {
        Ok(child) => child,
        Err(e) => {
            restore_terminal(&stdin, original_termios.as_ref());
            return Err(PtyError::Spawn(e));
        }
    };

    let signals = session_signals();
    signals.default_action.store(false, Ordering::SeqCst);
    signals.winch.store(false, Ordering::SeqCst);

    let result = run_io_loop(&pty, &mut child, &stdin, &signals.winch);

    restore_terminal(&stdin, original_termios.as_ref());
    signals.default_action.store(true, Ordering::SeqCst);

    result
}

fn to_pty_command(cmd: &Command) -> PtyCommand {
    let mut pty_cmd = PtyCommand::new(cmd.get_program()).args(cmd.get_args());
    for (key, value) in cmd.get_envs() {
        pty_cmd = match value {
            Some(value) => pty_cmd.env(key, value),
            None => pty_cmd.env_remove(key),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        pty_cmd = pty_cmd.current_dir(dir);
    }
    pty_cmd
}

fn restore_terminal(stdin: &Stdin, original: Option<&termios::Termios>) {
    if let Some(orig) = original {
        let _ = termios::tcsetattr(stdin, SetArg::TCSANOW, orig);
    }
}

/// Copy the terminal window size from stdin to the PTY.
fn copy_window_size(stdin: &Stdin, pty: &Pty) -> Option<(u16, u16)> {
    let ws = rustix::termios::tcgetwinsize(stdin).ok()?;
    let _ = pty.resize(Size::new_with_pixel(
        ws.ws_row,
        ws.ws_col,
        ws.ws_xpixel,
        ws.ws_ypixel,
    ));
    Some((ws.ws_col, ws.ws_row))
}

fn run_io_loop(pty: &Pty, child: &mut Child, stdin: &Stdin, winch: &AtomicBool) -> Result<()> {
    let mut detector = escape::EscapeDetector::new();

    let mut stdin_buf = [0u8; 8192];
    let mut master_buf = [0u8; 8192];
    let mut master = pty;

    loop {
        if let Some(status) = child.try_wait()? {
            debug!("Session ended with status: {}", status);
            break;
        }

        if winch.swap(false, Ordering::SeqCst)
            && let Some((cols, rows)) = copy_window_size(stdin, pty)
        {
            debug!("Window size updated: {}x{}", cols, rows);
        }

        let mut poll_fds = [
            PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
            PollFd::new(pty.as_fd(), PollFlags::POLLIN),
        ];

        match poll(&mut poll_fds, PollTimeout::from(100u16)) {
            Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }

        // Handle stdin -> master
        if let Some(revents) = poll_fds[0].revents() {
            if revents.intersects(PollFlags::POLLERR | PollFlags::POLLHUP) {
                break;
            }
            if revents.contains(PollFlags::POLLIN) {
                match std::io::stdin().read(&mut stdin_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &byte in &stdin_buf[..n] {
                            if detector.process(byte) {
                                info!("Escape sequence detected, disconnecting...");
                                eprintln!(
                                    "\r\n{}",
                                    "Connection closed by escape sequence.".yellow()
                                );
                                let _ = child.kill();
                                let _ = child.wait();
                                return Ok(());
                            }
                        }
                        let _ = master.write_all(&stdin_buf[..n]);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }

        // Handle master -> stdout
        if let Some(revents) = poll_fds[1].revents() {
            if revents.intersects(PollFlags::POLLERR | PollFlags::POLLHUP) {
                break;
            }
            if revents.contains(PollFlags::POLLIN) {
                match master.read(&mut master_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = std::io::stdout().write_all(&master_buf[..n]);
                        let _ = std::io::stdout().flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}
