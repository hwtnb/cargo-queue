use std::fmt;
use std::io;

/// Why cargo-queue ends without the command's own exit code.
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    message: String,
}

impl Failure {
    /// The arguments ask for something this system cannot do.
    pub const USAGE: i32 = 2;
    /// The command could not be started.
    pub const NOT_EXECUTABLE: i32 = 126;
    /// The command does not exist.
    pub const NOT_FOUND: i32 = 127;

    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Any other error, such as an unusable lock file, ends cargo-queue with 1.
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::new(1, error.to_string())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
