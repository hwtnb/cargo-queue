//! The machine-wide queue: an exclusive lock on one file, and a note naming
//! the run that holds it.
//!
//! The note lives in a separate `<lock file>.holder` file because Windows
//! locks are mandatory: while one process holds the lock, no other process
//! can read the locked file.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// How often a waiting run says that it still waits, and for whom.
const REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// The lock file named by `CARGO_QUEUE_LOCK`, or `~/.cache/cargo-queue.lock`.
pub fn lock_path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("CARGO_QUEUE_LOCK").filter(|path| !path.is_empty()) {
        return Ok(path.into());
    }
    let home = env::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot find the home directory; set CARGO_QUEUE_LOCK",
        )
    })?;
    Ok(home.join(".cache").join("cargo-queue.lock"))
}

/// Holds the lock; the next run in the queue starts once this is dropped.
pub struct Turn {
    /// The note this run wrote, or `None` when it could not write one.
    holder: Option<File>,
    _lock: File,
}

impl Drop for Turn {
    fn drop(&mut self) {
        // Runs before the fields drop and release the lock, so a later holder
        // that cannot write a note is never shown as this command.
        if let Some(holder) = &self.holder {
            let _ = holder.set_len(0);
        }
    }
}

/// Waits until no other run holds the lock at `path`, telling the user whom
/// it waits for, then records `note` for the runs that queue behind this one.
pub fn wait_for_turn(path: &Path, note: &str) -> io::Result<Turn> {
    let lock = open_lock(path)?;
    let holder = holder_path(path);
    let lock = match lock.try_lock() {
        Ok(()) => lock,
        Err(TryLockError::WouldBlock) => wait(lock, &holder, REPORT_INTERVAL, |message| {
            eprintln!("cargo-queue: {message}");
        })?,
        Err(TryLockError::Error(error)) => return Err(error),
    };
    Ok(Turn {
        holder: write_holder(&holder, note),
        _lock: lock,
    })
}

/// Blocks on `lock` and reports the wait: whom it waits for, again every
/// `interval` with the holder as it is then, and how long it took.
fn wait(
    lock: File,
    holder: &Path,
    interval: Duration,
    mut report: impl FnMut(String),
) -> io::Result<File> {
    report(format!("waiting for {}", read_holder(holder)));
    let waiting = Instant::now();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(lock.lock().map(|()| lock));
    });
    let mut next_report = waiting + interval;
    let lock = loop {
        match receiver.recv_timeout(next_report.saturating_duration_since(Instant::now())) {
            Ok(locked) => break locked?,
            Err(RecvTimeoutError::Timeout) => {
                report(format!(
                    "still waiting after {:.0}s for {}",
                    waiting.elapsed().as_secs_f64(),
                    read_holder(holder)
                ));
                next_report += interval;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the thread waiting for the lock ended"));
            }
        }
    };
    report(format!(
        "started after {:.0}s",
        waiting.elapsed().as_secs_f64()
    ));
    Ok(lock)
}

/// Opens the lock file read-only, which is all locking needs, so a sandbox
/// that denies writes to it still queues. Only a missing file needs writes.
fn open_lock(path: &Path) -> io::Result<File> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_lock(path).map_err(|error| {
            let message = format!(
                "cannot create {} ({error}); run cargo-queue once outside the sandbox to create it",
                path.display()
            );
            io::Error::new(error.kind(), message)
        }),
        result => result.map_err(|error| {
            io::Error::new(error.kind(), format!("cannot open {} ({error})", path.display()))
        }),
    }
}

fn create_lock(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn holder_path(lock: &Path) -> PathBuf {
    let mut path = OsString::from(lock);
    path.push(".holder");
    path.into()
}

fn read_holder(path: &Path) -> String {
    let note = fs::read_to_string(path).unwrap_or_default();
    match note.trim() {
        "" => "another build".to_owned(),
        note => note.to_owned(),
    }
}

/// Writes the note if this run may; a sandboxed run leaves waiters to print
/// "another build" instead.
fn write_holder(path: &Path, note: &str) -> Option<File> {
    let mut file = File::create(path).ok()?;
    file.write_all(note.as_bytes()).ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn a_waiting_run_keeps_naming_the_current_holder() {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let path = dir.path().join("cargo-queue.lock");
        let held = create_lock(&path).expect("create the lock file");
        held.lock().expect("take the lock");
        let holder = holder_path(&path);
        fs::write(&holder, "first\n").expect("write the note");

        let (sender, messages) = mpsc::channel();
        let waiter = {
            let lock = open_lock(&path).expect("open the lock file");
            let holder = holder.clone();
            thread::spawn(move || {
                wait(lock, &holder, Duration::from_millis(10), |message| {
                    sender.send(message).expect("collect the message");
                })
            })
        };
        let next = || messages.recv_timeout(TIMEOUT).expect("a message");

        assert_eq!(next(), "waiting for first");
        let still = next();
        assert!(still.starts_with("still waiting after "), "{still}");
        assert!(still.ends_with("s for first"), "{still}");

        fs::write(&holder, "second\n").expect("change the note");
        while !next().ends_with("s for second") {}
        held.unlock().expect("release the lock");
        let mut last = next();
        while last.starts_with("still waiting after ") {
            last = next();
        }
        assert!(last.starts_with("started after "), "{last}");
        waiter
            .join()
            .expect("the waiter")
            .expect("the lock after the wait");
    }
}
