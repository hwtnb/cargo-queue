//! The machine-wide queue: a directory of lock files, one per run that may
//! go at once.
//!
//! The directory holds:
//! - `N.lock`, held by the run in slot N for as long as its command runs;
//! - `N.holder`, the note naming that run for the runs that wait, written
//!   when the run starts. A note counts only while the pid it names runs:
//!   then it names the run in the slot, and its modification time is when
//!   that run started. It is a separate file because Windows locks are
//!   mandatory: while one process holds a lock, no other process can read
//!   the locked file;
//! - `gate.lock`, held while one run decides whether it may start, so no two
//!   runs decide at once.
//!
//! A run starts at once when no other run is running. Beside other runs, it
//! needs a free slot among the first `CARGO_QUEUE_MAX_RUNS`, every running
//! run to have recorded a start `ramp_up` ago or more, and memory to spare.
//! A run that could not write its note, as in a sandbox, records no start,
//! so no run starts beside it.

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::failure::Failure;
use crate::platform::{self, Memory};

/// How often the queue is checked, and how long to wait between checks.
struct Timing {
    /// Between checks of a waiting run.
    poll: Duration,
    /// Between the messages of a waiting run.
    report: Duration,
    /// How long after a start no other run starts beside it, while the
    /// memory the new run needs is still growing.
    ramp_up: Duration,
}

impl Timing {
    const REAL: Self = Self {
        poll: Duration::from_millis(250),
        report: Duration::from_secs(60),
        ramp_up: Duration::from_secs(30),
    };
}

/// How long a run tries for `gate.lock` before it counts as busy. Deciding
/// takes far less, so only a stopped holder keeps it longer.
const GATE_PATIENCE: Duration = Duration::from_millis(20);

#[derive(Clone)]
pub struct Queue {
    dir: PathBuf,
    max_runs: u32,
}

impl Queue {
    /// The queue in `CARGO_QUEUE_DIR`, or `~/.cache/cargo-queue`, with
    /// `CARGO_QUEUE_MAX_RUNS` slots, or one.
    pub fn from_env() -> Result<Self, Failure> {
        let dir = match env::var_os("CARGO_QUEUE_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => env::home_dir()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "cannot find the home directory; set CARGO_QUEUE_DIR",
                    )
                })?
                .join(".cache")
                .join("cargo-queue"),
        };
        let max_runs = match env::var_os("CARGO_QUEUE_MAX_RUNS").filter(|max| !max.is_empty()) {
            None => 1,
            Some(max) => max
                .to_str()
                .and_then(|max| max.parse::<NonZeroU32>().ok())
                .ok_or_else(|| {
                    Failure::new(
                        Failure::USAGE,
                        format!(
                            "CARGO_QUEUE_MAX_RUNS must be a positive whole number, not {}",
                            max.display()
                        ),
                    )
                })?
                .get(),
        };
        Ok(Self { dir, max_runs })
    }

    /// Creates every file the queue needs up to this run's maximum, so runs
    /// in a sandbox that cannot create them can still use every slot.
    pub fn set_up(&self) -> io::Result<()> {
        self.open_required("gate.lock")?;
        for number in 1..=self.max_runs {
            self.open_required(&format!("{number}.lock"))?;
        }
        Ok(())
    }

    /// Waits until this run may start, telling the user whom it waits for,
    /// then records `note` for the runs that queue behind this one.
    pub fn wait_for_turn(&self, note: &str) -> io::Result<Turn> {
        self.wait(note, &Timing::REAL, platform::memory, |message| {
            eprintln!("cargo-queue: {message}");
        })
    }

    /// Checks the queue every `timing.poll` until this run may start, and
    /// reports whom it waits for: at first, every `timing.report` after, and
    /// how long it took.
    fn wait(
        &self,
        note: &str,
        timing: &Timing,
        mut memory: impl FnMut() -> Memory,
        mut report: impl FnMut(String),
    ) -> io::Result<Turn> {
        let waiting = Instant::now();
        // The latest reason to wait; none yet while the gate stays busy.
        let mut blocked = None;
        let mut next_report = None;
        let turn = loop {
            match self.try_start(note, timing.ramp_up, &mut memory)? {
                Attempt::Started(turn) => break turn,
                Attempt::Blocked(reason) => blocked = Some(reason),
                Attempt::Busy => {}
            }
            let message = match next_report {
                None => Some(format!("waiting for {}", self.describe(blocked.as_ref())?)),
                Some(at) if Instant::now() >= at => Some(format!(
                    "still waiting after {:.0}s for {}",
                    waiting.elapsed().as_secs_f64(),
                    self.describe(blocked.as_ref())?
                )),
                Some(_) => None,
            };
            if let Some(message) = message {
                report(message);
                next_report = Some(next_report.unwrap_or(waiting) + timing.report);
            }
            thread::sleep(timing.poll);
        };
        if next_report.is_some() {
            report(format!(
                "started after {:.0}s",
                waiting.elapsed().as_secs_f64()
            ));
        }
        Ok(turn)
    }

    /// Takes a slot and records `note` in it if this run may start now.
    fn try_start(
        &self,
        note: &str,
        ramp_up: Duration,
        memory: &mut impl FnMut() -> Memory,
    ) -> io::Result<Attempt> {
        let Some(_gate) = self.enter_gate()? else {
            return Ok(Attempt::Busy);
        };
        let mut free = None;
        let mut running = Vec::new();
        for number in self.slot_numbers()? {
            let Some(lock) = self.open_slot(number)? else {
                continue;
            };
            match lock.try_lock() {
                Ok(()) if number <= self.max_runs && free.is_none() => free = Some((number, lock)),
                // Dropping the file releases the lock again.
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => running.push(number),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let blocked = |reason| {
            Ok(Attempt::Blocked(Blocked {
                slots: running.clone(),
                reason,
            }))
        };
        let Some((number, lock)) = free else {
            return blocked(Reason::Full);
        };
        if !running.is_empty() {
            let Some(latest) = running
                .iter()
                .map(|&number| self.note(number).map(|note| note.started))
                .collect::<Option<Vec<_>>>()
                .and_then(|starts| starts.into_iter().max())
            else {
                return blocked(Reason::Unrecorded);
            };
            // A start time ahead of the clock counts as just now.
            if SystemTime::now().duration_since(latest).unwrap_or_default() < ramp_up {
                return blocked(Reason::Starting);
            }
            match memory() {
                Memory::Spare => {}
                Memory::Tight => return blocked(Reason::Tight),
                Memory::Unknown => return blocked(Reason::Unknown),
            }
        }
        // Written before the gate opens, so the next run to decide sees
        // when this one started.
        Ok(Attempt::Started(Turn {
            holder: write_holder(&self.holder_path(number), note),
            _lock: lock,
        }))
    }

    /// Takes `gate.lock` unless another run keeps it for `GATE_PATIENCE`.
    fn enter_gate(&self) -> io::Result<Option<File>> {
        let gate = self.open_required("gate.lock")?;
        let deadline = Instant::now() + GATE_PATIENCE;
        loop {
            match gate.try_lock() {
                Ok(()) => return Ok(Some(gate)),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
    }

    /// The note of the run in slot `number`, if it wrote one. A note whose
    /// pid does not run was left by a run that ended without clearing it,
    /// and the run in the slot now could not write its own.
    fn note(&self, number: u32) -> Option<Note> {
        let mut file = File::open(self.holder_path(number)).ok()?;
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        let pid = text.strip_prefix("pid ")?.split(' ').next()?.parse().ok()?;
        if !platform::is_running(pid) {
            return None;
        }
        Some(Note {
            started: file.metadata().ok()?.modified().ok()?,
            text: text.trim().to_owned(),
        })
    }

    /// Whom a run waits for and why, for its messages. Without a decision
    /// yet, the runs whose notes count stand in.
    fn describe(&self, blocked: Option<&Blocked>) -> io::Result<String> {
        let holders: Vec<String> = match blocked {
            Some(blocked) => blocked
                .slots
                .iter()
                .map(|&number| {
                    self.note(number)
                        .map_or_else(|| "another build".to_owned(), |note| note.text)
                })
                .collect(),
            None => self
                .slot_numbers()?
                .into_iter()
                .filter_map(|number| self.note(number))
                .map(|note| note.text)
                .collect(),
        };
        let holders = match holders.as_slice() {
            [] => "another build".to_owned(),
            [holder] => holder.clone(),
            holders => format!("{} runs: {}", holders.len(), holders.join("; ")),
        };
        let reason = blocked.map_or("", |blocked| blocked.reason.suffix());
        Ok(format!("{holders}{reason}"))
    }

    /// The slots up to this run's maximum, and any other run's beyond it.
    fn slot_numbers(&self) -> io::Result<BTreeSet<u32>> {
        let mut numbers: BTreeSet<u32> = (1..=self.max_runs).collect();
        for entry in fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let number = name
                .to_str()
                .and_then(|name| name.strip_suffix(".lock"))
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|&number| number > 0 && name == format!("{number}.lock").as_str());
            numbers.extend(number);
        }
        Ok(numbers)
    }

    /// Opens slot `number`'s lock. Slot 1 must exist, as every run needs it;
    /// another slot that is missing and cannot be created, as in a sandbox,
    /// is left out.
    fn open_slot(&self, number: u32) -> io::Result<Option<File>> {
        let name = format!("{number}.lock");
        if number == 1 {
            return self.open_required(&name).map(Some);
        }
        let path = self.dir.join(name);
        match File::open(&path) {
            Ok(lock) => Ok(Some(lock)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((number <= self.max_runs)
                .then(|| create_lock(&path).ok())
                .flatten()),
            Err(error) => Err(cannot_open(&path, error)),
        }
    }

    /// Opens a lock file read-only, which is all locking needs, so a sandbox
    /// that denies writes to it still queues. Only a missing file needs
    /// writes.
    fn open_required(&self, name: &str) -> io::Result<File> {
        let path = self.dir.join(name);
        match File::open(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_lock(&path).map_err(|error| {
                    let message = format!(
                        "cannot create {} ({error}); run `cargo-queue --setup` once outside the sandbox",
                        path.display()
                    );
                    io::Error::new(error.kind(), message)
                })
            }
            result => result.map_err(|error| cannot_open(&path, error)),
        }
    }

    fn holder_path(&self, number: u32) -> PathBuf {
        self.dir.join(format!("{number}.holder"))
    }
}

/// The directory and how many slots it offers, as `--setup` reports them.
impl fmt::Display for Queue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slots = if self.max_runs == 1 { "slot" } else { "slots" };
        write!(f, "{} with {} {slots}", self.dir.display(), self.max_runs)
    }
}

/// Holds a slot; another run may take it once this is dropped.
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

/// A note that counts, as the pid it names runs.
struct Note {
    /// The note without its line ending, as waiting runs print it.
    text: String,
    /// When the run wrote it.
    started: SystemTime,
}

enum Attempt {
    Started(Turn),
    Blocked(Blocked),
    /// Another run has kept `gate.lock` for too long, so nothing was checked.
    Busy,
}

/// Why a run cannot start yet, and the slots of the runs it waits for.
struct Blocked {
    slots: Vec<u32>,
    reason: Reason,
}

enum Reason {
    /// Every slot this run may take is in use.
    Full,
    /// Another run did not record when it started, so it may have only
    /// just started.
    Unrecorded,
    /// Another run started too recently to tell how much memory it needs.
    Starting,
    /// A slot is free, but memory is not.
    Tight,
    /// A slot is free, but the system does not tell how much memory is.
    Unknown,
}

impl Reason {
    fn suffix(&self) -> &'static str {
        match self {
            Self::Full => "",
            Self::Unrecorded => " (another run did not record when it started)",
            Self::Starting => " (another run is still starting)",
            Self::Tight => " (memory is under pressure)",
            Self::Unknown => " (memory information is unavailable)",
        }
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

fn cannot_open(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot open {} ({error})", path.display()),
    )
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
    use std::process::{self, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    use tempfile::TempDir;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(30);
    /// Longer than any test, so a run that started stays starting.
    const FOREVER: Duration = Duration::from_secs(3600);

    fn queue_dir() -> TempDir {
        tempfile::tempdir().expect("create a temporary directory")
    }

    fn queue(dir: &TempDir, max_runs: u32) -> Queue {
        Queue {
            dir: dir.path().join("queue"),
            max_runs,
        }
    }

    fn timing(ramp_up: Duration) -> Timing {
        Timing {
            poll: Duration::from_millis(1),
            report: Duration::from_millis(10),
            ramp_up,
        }
    }

    /// A note as cargo-queue writes it, naming this running test process.
    fn note(command: &str) -> String {
        format!("pid {} in /work: {command}\n", process::id())
    }

    /// The note as waiting runs show it.
    fn shown(command: &str) -> String {
        note(command).trim().to_owned()
    }

    /// A run that has to start without waiting.
    fn start(queue: &Queue, command: &str) -> Turn {
        let report = |message| panic!("{command} had to wait: {message}");
        queue
            .wait(
                &note(command),
                &timing(Duration::ZERO),
                || Memory::Spare,
                report,
            )
            .expect("a turn")
    }

    /// A run that may wait, in its own thread.
    fn wait(queue: &Queue, command: &str, ramp_up: Duration, memory: Memory) -> Waiter {
        let (sender, messages) = mpsc::channel();
        let queue = queue.clone();
        let note = note(command);
        let run = thread::spawn(move || {
            queue.wait(
                &note,
                &timing(ramp_up),
                || memory,
                |message| {
                    sender.send(message).expect("collect the message");
                },
            )
        });
        Waiter { run, messages }
    }

    fn holder(queue: &Queue, number: u32) -> String {
        fs::read_to_string(queue.holder_path(number)).expect("read the note")
    }

    /// Makes the run in slot `number` look as if it started long ago.
    fn age(queue: &Queue, number: u32) {
        let note = File::options()
            .write(true)
            .open(queue.holder_path(number))
            .expect("open the note");
        note.set_modified(SystemTime::now() - 2 * FOREVER)
            .expect("backdate the note");
    }

    /// The pid of a process that has exited.
    fn ended_pid() -> u32 {
        let mut child = Command::new(env::current_exe().expect("this executable"))
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .expect("start a process");
        child.wait().expect("wait for it");
        child.id()
    }

    struct Waiter {
        run: JoinHandle<io::Result<Turn>>,
        messages: Receiver<String>,
    }

    impl Waiter {
        fn message(&self) -> String {
            self.messages.recv_timeout(TIMEOUT).expect("a message")
        }

        /// Skips messages until one ends with `suffix`.
        fn until(&self, suffix: &str) {
            while !self.message().ends_with(suffix) {}
        }

        /// Waits for the turn, whatever the run printed.
        fn turn(self) -> Turn {
            self.run.join().expect("the waiter").expect("a turn")
        }

        /// Waits for the turn and checks that the run reported its start.
        fn reported_turn(self) -> Turn {
            loop {
                let message = self.message();
                if !message.starts_with("still waiting after ") {
                    assert!(message.starts_with("started after "), "{message}");
                    return self.turn();
                }
            }
        }
    }

    #[test]
    fn a_waiting_run_keeps_naming_the_current_holder() {
        let dir = queue_dir();
        let queue = queue(&dir, 1);
        let first = start(&queue, "first");
        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(waiter.message(), format!("waiting for {}", shown("first")));
        let still = waiter.message();
        assert!(still.starts_with("still waiting after "), "{still}");
        assert!(
            still.ends_with(&format!("s for {}", shown("first"))),
            "{still}"
        );

        fs::write(queue.holder_path(1), note("renamed")).expect("change the note");
        waiter.until(&format!("s for {}", shown("renamed")));
        drop(first);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 1), note("second"));
    }

    #[test]
    fn a_lone_run_starts_whatever_the_memory() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let mut unknown = || Memory::Unknown;
        let attempt = queue
            .try_start(&note("first"), FOREVER, &mut unknown)
            .expect("an attempt");
        assert!(matches!(attempt, Attempt::Started(_)));
        assert_eq!(holder(&queue, 1), note("first"));
    }

    #[test]
    fn a_run_starts_beside_another_only_with_memory_to_spare() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        let tight = wait(&queue, "second", Duration::ZERO, Memory::Tight);
        assert_eq!(
            tight.message(),
            format!("waiting for {} (memory is under pressure)", shown("first"))
        );
        let unknown = wait(&queue, "third", Duration::ZERO, Memory::Unknown);
        assert_eq!(
            unknown.message(),
            format!(
                "waiting for {} (memory information is unavailable)",
                shown("first")
            )
        );
        drop(first);
        // One of them now runs alone; the other cannot start beside it.
        let (sender, started) = mpsc::channel();
        for waiter in [tight, unknown] {
            let sender = sender.clone();
            thread::spawn(move || sender.send(waiter.turn()));
        }
        let alone = started.recv_timeout(TIMEOUT).expect("one turn");
        assert!(started.recv_timeout(Duration::from_millis(50)).is_err());
        drop(alone);
        drop(started.recv_timeout(TIMEOUT).expect("the other turn"));

        let _fourth = start(&queue, "fourth");
        let _fifth = wait(&queue, "fifth", Duration::ZERO, Memory::Spare).turn();
        assert_eq!(holder(&queue, 2), note("fifth"));
    }

    #[test]
    fn a_run_waits_while_another_is_still_starting() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let _first = start(&queue, "first");
        let waiter = wait(&queue, "second", FOREVER, Memory::Spare);
        assert_eq!(
            waiter.message(),
            format!(
                "waiting for {} (another run is still starting)",
                shown("first")
            )
        );
        age(&queue, 1);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 2), note("second"));
    }

    /// A sandboxed run cannot write its note, so nothing tells when it
    /// started; even one that has run for long keeps others from starting.
    #[test]
    fn no_run_starts_beside_one_without_a_note() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        fs::write(queue.holder_path(1), "").expect("clear the note");
        age(&queue, 1);
        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(
            waiter.message(),
            "waiting for another build (another run did not record when it started)"
        );
        drop(first);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 1), note("second"));
    }

    /// A run that ended without clearing its note leaves it for a sandboxed
    /// run that takes the slot next and cannot write its own; the note
    /// neither names that run nor records its start.
    #[test]
    fn a_note_from_a_run_that_ended_counts_for_nothing() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        let stale = format!("pid {} in /work: gone\n", ended_pid());
        fs::write(queue.holder_path(1), &stale).expect("leave a stale note");
        age(&queue, 1);
        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(
            waiter.message(),
            "waiting for another build (another run did not record when it started)"
        );
        drop(first);
        let _second = waiter.reported_turn();
    }

    #[test]
    fn a_full_queue_names_every_run() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        let _second = wait(&queue, "second", Duration::ZERO, Memory::Spare).turn();

        let third = wait(&queue, "third", Duration::ZERO, Memory::Spare);
        assert_eq!(
            third.message(),
            format!(
                "waiting for 2 runs: {}; {}",
                shown("first"),
                shown("second")
            )
        );
        drop(first);
        let _third = third.reported_turn();
        assert_eq!(holder(&queue, 1), note("third"));
    }

    #[test]
    fn a_run_counts_the_slots_beyond_its_own_maximum() {
        let dir = queue_dir();
        let first = start(&queue(&dir, 2), "first");
        let second = wait(&queue(&dir, 2), "second", Duration::ZERO, Memory::Spare).turn();
        drop(first);

        let waiter = wait(&queue(&dir, 1), "third", Duration::ZERO, Memory::Tight);
        assert_eq!(
            waiter.message(),
            format!("waiting for {} (memory is under pressure)", shown("second"))
        );
        drop(second);
        let _third = waiter.reported_turn();
    }

    /// A run stopped while it decides keeps the gate; the others still say
    /// whom they wait for.
    #[test]
    fn a_busy_gate_does_not_silence_waiting_runs() {
        let dir = queue_dir();
        let queue = queue(&dir, 1);
        let first = start(&queue, "first");
        let gate = queue.open_required("gate.lock").expect("open the gate");
        gate.lock().expect("hold the gate");

        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(waiter.message(), format!("waiting for {}", shown("first")));
        waiter.until(&format!("s for {}", shown("first")));
        drop(first);
        waiter.until("s for another build");
        drop(gate);
        let _second = waiter.reported_turn();
    }
}
