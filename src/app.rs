//! What one run of cargo-queue does with its arguments.

use std::env;
use std::ffi::OsString;
use std::process;

use crate::child;
use crate::cli::{self, Invocation, Request, UsageError};
use crate::failure::Failure;
use crate::queue::Queue;

const HELP: &str = "\
Run one heavy build at a time across projects on this machine.

Usage:
  cargo queue [--background] <cargo arguments...>
  cargo-queue [--background] -- <command...>
  cargo-queue --setup | --help | --version

Waits for a lock in ~/.cache/cargo-queue (override with CARGO_QUEUE_DIR),
then runs the command and releases the lock when it exits.
The first form runs Cargo; the second runs any command, such as a script that
calls Cargo itself. --background lowers the command's priority: on macOS it
runs under `taskpolicy -b` (efficiency cores and low I/O priority), which can
make builds much slower; on Windows it uses the below-normal priority class.

Only commands started through this wrapper queue; editors, rust-analyzer and
plain `cargo` calls do not take the lock. A nested call inside a queued command
(CARGO_QUEUE_HELD=1) runs at once instead of waiting for itself. SIGINT, SIGTERM
and SIGHUP are passed to the command, and the lock is released only after it
exits. A command stopped with Ctrl-Z keeps the lock until it is resumed or ends.

CARGO_QUEUE_MAX_RUNS=N (default 1) lets up to N runs go at once. A run with
nothing else running always starts. Beside others, it starts only when each of
them recorded in its note a start at least 30 seconds ago and memory is to
spare; no run starts beside one that could not write its note.

The lock files are only read, so a sandbox that denies writes to them (such as
Codex's workspace-write mode) still queues, but the files must exist already:
run `cargo-queue --setup` outside the sandbox after installing and after
raising CARGO_QUEUE_MAX_RUNS. It creates them and runs nothing.
Waiters print the holders recorded in N.holder beside each N.lock, or \"another
build\" when a holder could not record itself, and print them again every
minute.

Exit status: the command's own; 128+N if signal N ended it; 127 if it was not
found; 126 if it could not be started; 2 for usage errors, including an
invalid CARGO_QUEUE_MAX_RUNS; 1 for other errors.
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
        Ok(Invocation::Setup) => set_up().unwrap_or_else(|failure| {
            eprintln!("cargo-queue: {failure}");
            failure.code
        }),
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

fn set_up() -> Result<i32, Failure> {
    let queue = Queue::from_env()?;
    queue.set_up()?;
    println!("cargo-queue: created {queue}");
    Ok(0)
}

fn queue_and_run(request: Request) -> Result<i32, Failure> {
    let command = child::command(request)?;
    // A nested call already holds the lock through its ancestor; waiting for
    // it would wait forever.
    let _turn = match env::var_os(child::HELD) {
        Some(held) if held == "1" => None,
        _ => Some(Queue::from_env()?.wait_for_turn(&holder_note(&command))?),
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
