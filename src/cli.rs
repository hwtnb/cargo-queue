//! The command line: `[queue] [--background] (-- <command...> | <cargo arguments...>)`.
//!
//! Only the leading options belong to cargo-queue. Everything from the first
//! other argument on goes to Cargo unchanged, so `cargo queue test --help`
//! shows Cargo's help and `cargo queue --offline build` keeps working.

use std::ffi::OsString;
use std::iter;

/// What one run of cargo-queue was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Version,
    Run(Request),
}

/// A command to run once the queue reaches it.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub priority: Priority,
    pub program: Program,
    pub args: Vec<OsString>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Priority {
    Normal,
    Background,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Program {
    /// `$CARGO`, which Cargo sets for its subcommands, or else `cargo`.
    Cargo,
    /// A command given after `--`.
    Named(OsString),
}

/// The arguments name no command to run.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError;

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, UsageError> {
    let mut args = args.into_iter().peekable();
    // `cargo queue ...` makes Cargo run `cargo-queue queue ...`.
    args.next_if(|arg| arg == "queue");
    let mut priority = Priority::Normal;
    while let Some(arg) = args.next() {
        let (program, args) = match arg.to_str() {
            Some("--background") => {
                priority = Priority::Background;
                continue;
            }
            Some("-h" | "--help") => return Ok(Invocation::Help),
            Some("-V" | "--version") => return Ok(Invocation::Version),
            Some("--") => (
                Program::Named(args.next().ok_or(UsageError)?),
                args.collect(),
            ),
            _ => (Program::Cargo, iter::once(arg).chain(args).collect()),
        };
        return Ok(Invocation::Run(Request {
            priority,
            program,
            args,
        }));
    }
    Err(UsageError)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Invocation, UsageError> {
        parse(args.iter().map(OsString::from))
    }

    fn run(priority: Priority, program: Program, args: &[&str]) -> Result<Invocation, UsageError> {
        let args = args.iter().map(OsString::from).collect();
        Ok(Invocation::Run(Request {
            priority,
            program,
            args,
        }))
    }

    fn named(program: &str) -> Program {
        Program::Named(program.into())
    }

    #[test]
    fn arguments_without_a_command_are_a_usage_error() {
        for args in [
            &[][..],
            &["queue"],
            &["--background"],
            &["queue", "--background"],
            &["--"],
        ] {
            assert_eq!(parse_strs(args), Err(UsageError), "{args:?}");
        }
    }

    #[test]
    fn leading_options_belong_to_cargo_queue() {
        use Invocation::{Help, Version};
        use Priority::{Background, Normal};
        let cases: &[(&[&str], Result<Invocation, UsageError>)] = &[
            (&["-h"], Ok(Help)),
            (&["queue", "--help"], Ok(Help)),
            (&["-V"], Ok(Version)),
            (&["queue", "--version"], Ok(Version)),
            (&["--background", "--help"], Ok(Help)),
            (
                &["--background", "test"],
                run(Background, Program::Cargo, &["test"]),
            ),
            (
                &["queue", "--background", "--", "sh"],
                run(Background, named("sh"), &[]),
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(&parse_strs(args), expected, "{args:?}");
        }
        assert_eq!(
            parse_strs(&["build"]),
            run(Normal, Program::Cargo, &["build"])
        );
    }

    #[test]
    fn everything_after_the_first_other_argument_passes_through() {
        use Priority::Normal;
        let cases: &[(&[&str], Result<Invocation, UsageError>)] = &[
            (
                &["queue", "test", "--", "--nocapture"],
                run(Normal, Program::Cargo, &["test", "--", "--nocapture"]),
            ),
            (
                &["test", "--help"],
                run(Normal, Program::Cargo, &["test", "--help"]),
            ),
            (
                &["test", "--background"],
                run(Normal, Program::Cargo, &["test", "--background"]),
            ),
            (
                &["--offline", "build"],
                run(Normal, Program::Cargo, &["--offline", "build"]),
            ),
            (&["queue", "queue"], run(Normal, Program::Cargo, &["queue"])),
            (
                &["--", "sh", "-c", "exit 3"],
                run(Normal, named("sh"), &["-c", "exit 3"]),
            ),
            (&["--", "--help"], run(Normal, named("--help"), &[])),
        ];
        for (args, expected) in cases {
            assert_eq!(&parse_strs(args), expected, "{args:?}");
        }
    }
}
