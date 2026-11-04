//! Runs the queued command, passes interruptions on to it, and reports how it
//! ended.

use std::env;
use std::io;
use std::iter;

use tokio::process::Command;

use crate::cli::{Priority, Program, Request};
use crate::failure::Failure;
use crate::platform::{self, Supervision};

/// Set for the command, so a nested cargo-queue knows the lock is held.
pub const HELD: &str = "CARGO_QUEUE_HELD";

pub fn command(request: Request) -> Result<Command, Failure> {
    let program = match request.program {
        Program::Cargo => env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
        Program::Named(program) => program,
    };
    let mut command = match request.priority {
        Priority::Normal => Command::new(program),
        Priority::Background => platform::background(program)?,
    };
    command.args(request.args).env(HELD, "1");
    Ok(command)
}

/// The command line as the holder note shows it.
pub fn describe(command: &Command) -> String {
    let command = command.as_std();
    let words: Vec<_> = iter::once(command.get_program())
        .chain(command.get_args())
        .map(|word| word.to_string_lossy())
        .collect();
    words.join(" ")
}

/// Runs `command` to completion and returns the exit code to end with.
pub fn run(command: Command) -> Result<i32, Failure> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(supervise(command))
}

async fn supervise(mut command: Command) -> Result<i32, Failure> {
    // Started before the spawn: an interruption that arrives in between waits
    // here and reaches the child, instead of ending cargo-queue alone.
    let mut supervision = Supervision::start()?;
    let mut child = command.spawn().map_err(|error| {
        let code = match error.kind() {
            io::ErrorKind::NotFound => Failure::NOT_FOUND,
            _ => Failure::NOT_EXECUTABLE,
        };
        Failure::new(
            code,
            format!(
                "cannot run {}: {error}",
                command.as_std().get_program().display()
            ),
        )
    })?;
    loop {
        tokio::select! {
            status = child.wait() => return Ok(platform::exit_code(status?)),
            interruption = supervision.next_interruption() => supervision.forward(&child, interruption),
        }
    }
}
