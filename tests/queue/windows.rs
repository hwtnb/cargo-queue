//! Windows: the job object and `--background`.
//!
//! Ctrl-C is not tested. Generating it needs a console shared with the test,
//! and the event would reach the test runner too.

use std::process::{Command, Stdio};

use libtest_mimic::Trial;
use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetPriorityClass, OpenProcess,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use crate::child::ROLE;
use crate::support::{Fixture, Run, TIMEOUT, child_exe};

pub fn trials() -> Vec<Trial> {
    trials![
        terminating_cargo_queue_ends_the_command,
        background_lowers_the_priority_class,
    ]
}

/// Terminates the process without letting it clean up, ignoring one that has
/// already exited.
pub fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The `print-priority` role.
pub fn print_priority() -> i32 {
    // SAFETY: the current-process pseudo handle is always valid.
    let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
    let name = if class == BELOW_NORMAL_PRIORITY_CLASS {
        "below normal"
    } else {
        "other"
    };
    println!("{name}");
    0
}

/// Waits until the process exits; true unless it is still running at the
/// timeout.
fn exits_in_time(pid: u32) -> bool {
    // SAFETY: OpenProcess takes plain values and returns null for a process
    // that no longer exists.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return true;
    }
    let millis = u32::try_from(TIMEOUT.as_millis()).expect("a timeout of seconds");
    // SAFETY: `process` is a valid handle opened with SYNCHRONIZE access.
    let exited = unsafe { WaitForSingleObject(process, millis) } == WAIT_OBJECT_0;
    // SAFETY: `process` is a valid handle that is not used again.
    unsafe { CloseHandle(process) };
    exited
}

fn terminating_cargo_queue_ends_the_command() {
    let fixture = Fixture::new();
    let (holder, held) = fixture.hold(&[]);
    kill(holder.pid());
    assert!(exits_in_time(held.pid), "the command outlived cargo-queue");
    let next = fixture.start_child("exit", &["0"]).finish();
    assert!(next.status.success());
    assert!(next.stderr.is_empty(), "{:?}", next.stderr);
}

fn background_lowers_the_priority_class() {
    let fixture = Fixture::new();
    let exe = child_exe();
    let mut command = fixture.cargo_queue(&["--background", "--", &exe]);
    command.env(ROLE, "print-priority");
    let run = Run::start(command).finish();
    assert!(run.status.success(), "{:?}", run.stderr);
    assert_eq!(run.stdout, ["below normal"]);
}
