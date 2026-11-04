//! Unix: SIGINT, SIGTERM and SIGHUP go on to the command, and cargo-queue
//! stays until the command exits.

use std::ffi::{OsString, c_int};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::failure::Failure;

/// Catches the termination signals that would otherwise end cargo-queue
/// while the command runs on without the lock.
pub struct Supervision {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

/// A caught signal number.
pub type Interruption = c_int;

impl Supervision {
    pub fn start() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    pub async fn next_interruption(&mut self) -> Interruption {
        tokio::select! {
            _ = self.interrupt.recv() => libc::SIGINT,
            _ = self.terminate.recv() => libc::SIGTERM,
            _ = self.hangup.recv() => libc::SIGHUP,
        }
    }

    pub fn forward(&self, child: &Child, signal: Interruption) {
        // `id` is `None` once `wait` has reaped the child, so the pid cannot
        // belong to another process by now.
        if let Some(pid) = child.id() {
            // SAFETY: kill takes plain integers. It fails only with ESRCH when
            // the child has exited but is not reaped yet, which needs nothing.
            unsafe { libc::kill(pid as libc::pid_t, signal) };
        }
    }
}

/// The command's exit code, or 128 + N when signal N ended it, as shells
/// report it.
pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        128 + status
            .signal()
            .expect("a reaped child has either exited or been killed by a signal")
    })
}

/// Runs the program under `taskpolicy -b`: efficiency cores and low I/O
/// priority.
#[cfg(target_os = "macos")]
pub fn background(program: OsString) -> Result<Command, Failure> {
    let mut command = Command::new("taskpolicy");
    command.arg("-b").arg(program);
    Ok(command)
}

#[cfg(not(target_os = "macos"))]
pub fn background(_program: OsString) -> Result<Command, Failure> {
    Err(Failure::new(
        Failure::USAGE,
        "--background is supported only on macOS and Windows",
    ))
}
