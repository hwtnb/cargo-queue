//! Unix: signals reach the command, and the sandbox and `--background`
//! behavior of Unix systems.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use libtest_mimic::Trial;

use crate::support::{Fixture, Run};

pub fn trials() -> Vec<Trial> {
    trials![
        a_command_ended_by_a_signal_exits_with_128_plus_its_number,
        sigterm_reaches_the_command_and_the_lock_outlasts_it,
        a_signal_during_startup_never_leaves_the_command_running,
        a_stopped_command_keeps_the_lock,
        a_missing_lock_file_in_a_read_only_directory_is_explained,
        background,
    ]
}

pub fn kill(pid: u32) {
    send_signal(pid, "KILL");
}

fn send_signal(pid: impl ToString, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -{signal} failed");
}

fn is_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .expect("run kill")
        .success()
}

/// Starts `script` under cargo-queue and returns once it printed its first
/// line.
fn start_script(fixture: &Fixture, script: &str) -> (Run, String) {
    let mut run = fixture.start(&["--", "sh", "-c", script]);
    let first = run.stdout_line();
    (run, first)
}

fn a_command_ended_by_a_signal_exits_with_128_plus_its_number() {
    let fixture = Fixture::new();
    let run = fixture.start(&["--", "sh", "-c", "kill -TERM $$"]).finish();
    assert_eq!(run.status.code(), Some(128 + 15));
}

fn sigterm_reaches_the_command_and_the_lock_outlasts_it() {
    let fixture = Fixture::new();
    let script = "trap 'echo terminating; sleep 0.2; echo exited >> log; exit 0' TERM; \
                  echo started; while :; do sleep 0.05; done";
    let (mut holder, _) = start_script(&fixture, script);
    let mut waiter = fixture.start(&["--", "sh", "-c", "echo waiter >> log"]);
    assert!(
        waiter
            .stderr_line()
            .starts_with("cargo-queue: waiting for ")
    );

    send_signal(holder.pid(), "TERM");
    assert_eq!(holder.stdout_line(), "terminating");
    let held = holder.finish();
    // The trap's `exit 0`: cargo-queue waited for the command instead of dying.
    assert_eq!(held.status.code(), Some(0));
    assert!(waiter.finish().status.success());
    assert_eq!(fixture.read("log"), "exited\nwaiter\n");
}

/// Sends SIGTERM at spread-out moments of startup. Measured on an M-series
/// Mac, a signal sent at once always ends cargo-queue before it catches
/// signals, and one sent 10 ms later always reaches the command. The window
/// between catching signals and starting the command lasts microseconds, so
/// this test cannot single it out; `supervise` keeps that order by design.
/// A loaded machine starts more slowly, so the delays keep growing until a
/// signal has reached the command at least once.
fn a_signal_during_startup_never_leaves_the_command_running() {
    let fixture = Fixture::new();
    let mut forwarded = 0;
    for attempt in 0.. {
        if attempt >= 20 && forwarded > 0 {
            break;
        }
        assert!(attempt < 100, "no signal arrived after startup");
        let pidfile = format!("pid-{attempt}");
        let script = format!("echo $$ > {pidfile}; exec sleep 30");
        let run = fixture.start(&["--", "sh", "-c", &script]);
        thread::sleep(Duration::from_millis(5 * attempt));
        send_signal(run.pid(), "TERM");
        let status = run.finish().status;
        // Either cargo-queue died before it caught signals, and so before it
        // started the command, or the command received the signal.
        match (status.signal(), status.code()) {
            (Some(15), _) => {}
            (_, Some(143)) => forwarded += 1,
            _ => panic!("attempt {attempt}: {status:?}"),
        }
        let pid = fixture.read(&pidfile);
        let pid = pid.trim();
        assert!(
            pid.is_empty() || !is_alive(pid),
            "attempt {attempt}: command {pid} outlived cargo-queue"
        );
    }
}

fn a_stopped_command_keeps_the_lock() {
    let fixture = Fixture::new();
    let (holder, held) = fixture.hold(&[]);
    send_signal(held.pid, "STOP");
    let mut waiter = fixture.start_child("exit", &["0"]);
    assert!(
        waiter
            .stderr_line()
            .starts_with("cargo-queue: waiting for ")
    );
    send_signal(held.pid, "CONT");
    assert!(holder.finish().status.success());
    assert!(waiter.finish().status.success());
}

fn a_missing_lock_file_in_a_read_only_directory_is_explained() {
    let fixture = Fixture::new();
    let _sealed = Sealed::new(fixture.path("queue"));
    let run = fixture.start(&["--", "true"]).finish();
    assert_eq!(run.status.code(), Some(1));
    let message = &run.stderr[0];
    let expected = format!("cargo-queue: cannot create {}", fixture.queue().display());
    assert!(message.starts_with(&expected), "{message}");
    assert!(
        message.ends_with("run `cargo-queue --setup` once outside the sandbox"),
        "{message}"
    );
}

/// Makes a directory read-only, as a sandbox does, for as long as it lives.
struct Sealed(PathBuf);

impl Sealed {
    fn new(dir: PathBuf) -> Self {
        fs::set_permissions(&dir, Permissions::from_mode(0o555)).expect("seal the directory");
        Self(dir)
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, Permissions::from_mode(0o755));
    }
}

#[cfg(target_os = "macos")]
fn background() {
    let fixture = Fixture::new();
    // The scheduling priority `ps` shows drops under `taskpolicy -b`.
    let priority = |options: &[&str]| {
        let script = ["--", "sh", "-c", "ps -o pri= -p $$"];
        let run = fixture.start(&[options, &script].concat()).finish();
        assert!(run.status.success(), "{:?}", run.stderr);
        run.stdout[0]
            .trim()
            .parse::<u32>()
            .expect("a scheduling priority")
    };
    let normal = priority(&[]);
    let background = priority(&["--background"]);
    assert!(background < normal, "{background} is not below {normal}");
}

#[cfg(not(target_os = "macos"))]
fn background() {
    let fixture = Fixture::new();
    let run = fixture.start(&["--background", "--", "true"]).finish();
    assert_eq!(run.status.code(), Some(2));
    assert_eq!(
        run.stderr,
        ["cargo-queue: --background is supported only on macOS and Windows"]
    );
}
