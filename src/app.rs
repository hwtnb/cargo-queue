//! What one run of cargo-queue does with its arguments.

use std::env;
use std::ffi::OsString;
use std::process;

use crate::cli::{self, Invocation, Request, UsageError};
use crate::failure::Failure;
use crate::{child, queue};

const HELP: &str = "\
Run one heavy build at a time across projects on this machine.

Usage:
  cargo queue [--background] <cargo arguments...>
  cargo-queue [--background] -- <command...>
  cargo-queue --help | --version

Waits for an exclusive lock on ~/.cache/cargo-queue.lock (override with
CARGO_QUEUE_LOCK), then runs the command and releases the lock when it exits.
The first form runs Cargo; the second runs any command, such as a script that
calls Cargo itself. --background lowers the command's priority: on macOS it
runs under `taskpolicy -b` (efficiency cores and low I/O priority), which can
make builds much slower; on Windows it uses the below-normal priority class.

Only commands started through this wrapper queue; editors, rust-analyzer and
plain `cargo` calls do not take the lock. A nested call inside a queued command
(CARGO_QUEUE_HELD=1) runs at once instead of waiting for itself. SIGINT, SIGTERM
and SIGHUP are passed to the command, and the lock is released only after it
exits. A command stopped with Ctrl-Z keeps the lock until it is resumed or ends.

The lock file is only read, so a sandbox that denies writes to it (such as
Codex's workspace-write mode) still queues, but the file must exist already.
Waiters print the holder recorded in <lock file>.holder, or \"another build\"
when the holder could not record itself, and print it again every minute.

Exit status: the command's own; 128+N if signal N ended it; 127 if it was not
found; 126 if it could not be started; 2 for usage errors; 1 for other errors.
";

/// Carries out the arguments after the program name and returns the exit
/// code. Everything that cleans up on drop is gone when this returns.
pub fn run(args: impl IntoIterator<Item = OsString>) -> i32 {
    match cli::parse(args) {
        Ok(Invocation::Help) => {
            print!("{HELP}");
            0
        }
        Ok(Invocation::Version) => {
            println!("cargo-queue {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(Invocation::Run(request)) => queue_and_run(request).unwrap_or_else(|failure| {
            eprintln!("cargo-queue: {failure}");
            failure.code
        }),
        Err(UsageError) => {
            eprint!("{HELP}");
            Failure::USAGE
        }
    }
}

fn queue_and_run(request: Request) -> Result<i32, Failure> {
    let command = child::command(request)?;
    // A nested call already holds the lock through its ancestor; waiting for
    // it would wait forever.
    let _turn = match env::var_os(child::HELD) {
        Some(held) if held == "1" => None,
        _ => Some(queue::wait_for_turn(
            &queue::lock_path()?,
            &holder_note(&command),
        )?),
    };
    child::run(command)
}

fn holder_note(command: &tokio::process::Command) -> String {
    let directory = env::current_dir().map_or_else(
        |_| "an unknown directory".to_owned(),
        |directory| directory.display().to_string(),
    );
    format!(
        "pid {} in {directory}: {}\n",
        process::id(),
        child::describe(command)
    )
}
