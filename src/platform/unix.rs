//! Unix: SIGINT, SIGTERM and SIGHUP go on to the command, and cargo-queue
//! stays until the command exits.

use std::ffi::{OsString, c_int};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::failure::Failure;
use crate::platform::Memory;

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

/// Whether the process with this pid is known to be running: false once it
/// has exited, even before its parent reaps it, and when the system will
/// not say.
#[cfg(target_os = "macos")]
pub fn is_running(pid: u32) -> bool {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).expect("a small structure");
    // SAFETY: `info` is a writable buffer of `size` bytes. With argument 0,
    // a process that has exited is not found even before it is reaped.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    written == size
}

/// Whether the process with this pid is known to be running: false once it
/// has exited, even before its parent reaps it, and when the system will
/// not say.
#[cfg(target_os = "linux")]
pub fn is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| linux::is_running_state(&stat))
}

/// Whether a process with this pid exists. Other systems keep a process
/// that has exited until its parent reaps it, and this cannot tell; it
/// decides only what waiting runs print, as memory is never spare here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn is_running(pid: u32) -> bool {
    // Zero would address this process group, not a process.
    let Some(pid) = libc::pid_t::try_from(pid).ok().filter(|&pid| pid > 0) else {
        return false;
    };
    // SAFETY: kill takes plain integers, and signal 0 only checks that the
    // process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    // EPERM: the process exists but belongs to someone else.
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Spare when memory pressure is normal and at least a quarter of memory is
/// free; the Codex sandbox hides both.
#[cfg(target_os = "macos")]
pub fn memory() -> Memory {
    /// `kern.memorystatus_vm_pressure_level` when memory is not under
    /// pressure; 2 means warn and 4 critical.
    const NORMAL: libc::c_int = 1;
    let (Some(pressure), Some(free)) = (
        sysctl(c"kern.memorystatus_vm_pressure_level"),
        // The free percentage that `memory_pressure` prints.
        sysctl(c"kern.memorystatus_level").and_then(|free| u64::try_from(free).ok()),
    ) else {
        return Memory::Unknown;
    };
    if pressure == NORMAL {
        Memory::judge(free, 100)
    } else {
        Memory::Tight
    }
}

#[cfg(target_os = "macos")]
fn sysctl(name: &std::ffi::CStr) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut size = size_of_val(&value);
    // SAFETY: `name` is NUL-terminated, `value` and `size` describe a buffer
    // of the right size for an integer value, and nothing is written.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && size == size_of_val(&value)).then_some(value)
}

/// Spare when at least a quarter of memory is available and tasks have
/// barely stalled on memory lately. Without pressure stall information, as
/// on older kernels, available memory alone decides.
#[cfg(target_os = "linux")]
pub fn memory() -> Memory {
    use std::fs;
    let Some((available, total)) = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|info| linux::available_and_total(&info))
    else {
        return Memory::Unknown;
    };
    let stalled = fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|pressure| linux::stalled_percent(&pressure));
    match Memory::judge(available, total) {
        Memory::Spare if stalled.is_some_and(|stalled| stalled >= linux::STALL_LIMIT) => {
            Memory::Tight
        }
        memory => memory,
    }
}

#[cfg(target_os = "linux")]
mod linux {
    /// The share of the last ten seconds, in percent, in which some task
    /// waited for memory, from which no build starts beside others.
    pub const STALL_LIMIT: f64 = 10.0;

    /// `MemAvailable` and `MemTotal` from `/proc/meminfo`, in kB.
    pub fn available_and_total(meminfo: &str) -> Option<(u64, u64)> {
        let field = |name: &str| {
            meminfo.lines().find_map(|line| {
                let value = line.strip_prefix(name)?.strip_prefix(':')?;
                value.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
            })
        };
        Some((field("MemAvailable")?, field("MemTotal")?))
    }

    /// Whether `/proc/<pid>/stat` shows a process that has not exited. The
    /// state follows the command name, which may itself hold parentheses.
    pub fn is_running_state(stat: &str) -> bool {
        let state = stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next());
        state.is_some_and(|state| !matches!(state, 'Z' | 'X' | 'x'))
    }

    /// `avg10` of the `some` line in `/proc/pressure/memory`.
    pub fn stalled_percent(pressure: &str) -> Option<f64> {
        let some = pressure
            .lines()
            .find_map(|line| line.strip_prefix("some "))?;
        some.split_whitespace()
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse()
            .ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_available_memory() {
            let meminfo = "MemTotal:       16000000 kB\nMemFree:          100000 kB\n\
                           MemAvailable:    4000000 kB\n";
            assert_eq!(available_and_total(meminfo), Some((4_000_000, 16_000_000)));
            assert_eq!(available_and_total("MemTotal: 100 kB\n"), None);
        }

        #[test]
        fn reads_the_recent_memory_stall() {
            let pressure = "some avg10=12.50 avg60=3.00 avg300=1.00 total=100\n\
                            full avg10=2.00 avg60=1.00 avg300=0.50 total=50\n";
            assert_eq!(stalled_percent(pressure), Some(12.5));
            assert_eq!(stalled_percent(""), None);
        }

        #[test]
        fn reads_whether_a_process_has_exited() {
            assert!(is_running_state("42 (cargo) S 1 42 42 0 -1"));
            assert!(is_running_state("42 (a) (b) R 1 42 42 0 -1"));
            assert!(!is_running_state("42 (cargo) Z 1 42 42 0 -1"));
            assert!(!is_running_state("42 (cargo) X 1 42 42 0 -1"));
            assert!(!is_running_state(""));
        }
    }
}

/// Other systems are not read, so no build starts beside another.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn memory() -> Memory {
    Memory::Unknown
}

/// Only these systems tell a process that has exited from a running one.
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::env;
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn an_exited_process_is_not_running_before_it_is_reaped() {
        let mut child = Command::new(env::current_exe().expect("this executable"))
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .expect("start a process");
        let pid = libc::id_t::from(child.id());
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `info` is a writable siginfo_t; WNOWAIT leaves the child
        // for `child.wait` to reap.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0, "{}", io::Error::last_os_error());
        assert!(!is_running(child.id()));
        child.wait().expect("reap it");
    }
}
