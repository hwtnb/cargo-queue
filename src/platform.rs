//! The OS-specific parts of running the command, under one set of names.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Supervision, background, exit_code};
#[cfg(windows)]
pub use windows::{Supervision, background, exit_code};
