//! The OS-specific parts of running the command, under one set of names.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Supervision, background, exit_code, is_running, memory};
#[cfg(windows)]
pub use windows::{Supervision, background, exit_code, is_running, memory};

/// Whether the system has memory for another build beside running ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memory {
    Spare,
    Tight,
    /// The system would not say, as in a sandbox that hides it.
    Unknown,
}

impl Memory {
    /// Spare when at least a quarter of `total` is `available`.
    fn judge(available: u64, total: u64) -> Self {
        /// The share of memory, in percent, that must be free.
        const SPARE_PERCENT: u128 = 25;
        if total == 0 {
            Self::Unknown
        } else if u128::from(available) * 100 / u128::from(total) >= SPARE_PERCENT {
            Self::Spare
        } else {
            Self::Tight
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quarter_of_memory_is_enough() {
        assert_eq!(Memory::judge(25, 100), Memory::Spare);
        assert_eq!(Memory::judge(24, 100), Memory::Tight);
        assert_eq!(Memory::judge(4 << 30, 16 << 30), Memory::Spare);
        assert_eq!(Memory::judge(u64::MAX, u64::MAX), Memory::Spare);
        assert_eq!(Memory::judge(0, 0), Memory::Unknown);
    }

    #[test]
    fn this_process_is_running() {
        assert!(is_running(std::process::id()));
    }

    /// The real reading. A sandbox may hide it on macOS; the systems in CI
    /// show it.
    #[test]
    fn the_system_reports_its_memory() {
        let memory = memory();
        if cfg!(any(target_os = "linux", windows)) {
            assert_ne!(memory, Memory::Unknown);
        }
    }
}
