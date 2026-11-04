//! A private lock per test, and cargo-queue runs whose output arrives line by
//! line.

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::child::ROLE;

/// Long enough for a loaded machine; reached only when a test fails.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// This test executable, which plays the commands' roles.
pub fn child_exe() -> String {
    let exe = env::current_exe().expect("this executable");
    exe.to_str().expect("a UTF-8 path").to_owned()
}

/// A private directory with its own lock file under `queue/`.
pub struct Fixture {
    dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        fs::create_dir(dir.path().join("queue")).expect("create the lock directory");
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn lock(&self) -> PathBuf {
        self.path("queue/cargo-queue.lock")
    }

    pub fn holder(&self) -> PathBuf {
        self.path("queue/cargo-queue.lock.holder")
    }

    pub fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// cargo-queue as called from outside any queued command.
    pub fn cargo_queue(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-queue"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("CARGO_QUEUE_LOCK", self.lock())
            // Set when the tests themselves run under `cargo queue test`.
            .env_remove("CARGO_QUEUE_HELD");
        command
    }

    pub fn start(&self, args: &[&str]) -> Run {
        Run::start(self.cargo_queue(args))
    }

    /// cargo-queue running this executable in `role`.
    pub fn child(&self, role: &str, args: &[&str]) -> Command {
        let exe = child_exe();
        let mut command = self.cargo_queue(&[&["--", exe.as_str()][..], args].concat());
        command.env(ROLE, role);
        command
    }

    pub fn start_child(&self, role: &str, args: &[&str]) -> Run {
        Run::start(self.child(role, args))
    }

    /// Starts a run whose command holds the lock until `Run::release`, and
    /// returns once that command runs.
    pub fn hold(&self, args: &[&str]) -> (Run, Held) {
        let mut run = self.start_child("hold", args);
        let pid = run.stdout_line().parse().expect("the command's pid");
        let cwd = run.stdout_line();
        (run, Held { pid, cwd })
    }
}

/// The command a `hold` run started.
pub struct Held {
    pub pid: u32,
    /// The working directory as the command, and so cargo-queue, sees it.
    pub cwd: String,
}

/// Marks a file read-only for as long as it lives.
pub struct ReadOnly(PathBuf);

impl ReadOnly {
    pub fn new(path: PathBuf) -> Self {
        set_readonly(&path, true);
        Self(path)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        // Windows cannot delete a read-only file with the temporary directory.
        set_readonly(&self.0, false);
    }
}

fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = fs::metadata(path).expect("read permissions").permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions).expect("change permissions");
}

/// A running cargo-queue whose output arrives line by line.
pub struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
}

impl Run {
    pub fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start cargo-queue");
        let stdin = child.stdin.take();
        let stdout = lines(child.stdout.take().expect("piped stdout"));
        let stderr = lines(child.stderr.take().expect("piped stderr"));
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn stdout_line(&mut self) -> String {
        self.stdout.recv_timeout(TIMEOUT).expect("a line on stdout")
    }

    pub fn stderr_line(&mut self) -> String {
        self.stderr.recv_timeout(TIMEOUT).expect("a line on stderr")
    }

    /// Closes stdin, which lets a `hold` command finish.
    pub fn release(&mut self) {
        self.stdin.take();
    }

    /// Waits for the run to end and returns its status and remaining output.
    pub fn finish(mut self) -> Finished {
        self.release();
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll cargo-queue") {
                break status;
            }
            assert!(Instant::now() < deadline, "cargo-queue did not finish");
            thread::sleep(Duration::from_millis(10));
        };
        Finished {
            status,
            stdout: self.stdout.iter().collect(),
            stderr: self.stderr.iter().collect(),
        }
    }
}

pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

fn lines(stream: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}
