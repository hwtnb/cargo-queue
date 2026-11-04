//! Runs one heavy build at a time across projects on this machine.

mod app;
mod child;
mod cli;
mod failure;
mod platform;
mod queue;

fn main() {
    let code = app::run(std::env::args_os().skip(1));
    // `exit` rather than returning an `ExitCode`, which cannot carry Windows'
    // 32-bit exit codes.
    std::process::exit(code)
}
