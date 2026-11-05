//! Runs the built cargo-queue against a private queue per test.
//!
//! The commands cargo-queue runs here are this test executable itself in a
//! child role (see `child`), so the tests need no shell and run on Windows
//! too. The custom harness lets the executable take that role before it
//! parses any test arguments. Only tests about Unix signals script their
//! commands with `sh`.

/// Registers test functions under their own names.
macro_rules! trials {
    ($($test:ident),* $(,)?) => {
        vec![$(libtest_mimic::Trial::test(stringify!($test), || {
            $test();
            Ok(())
        })),*]
    };
}

mod child;
mod portable;
mod support;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;

fn main() {
    child::run_if_requested();
    let mut trials = portable::trials();
    trials.extend(os::trials());
    libtest_mimic::run(&libtest_mimic::Arguments::from_args(), trials).exit()
}
