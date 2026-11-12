//! The machine-wide queue: a directory of lock files, one per run that may
//! go at once.
//!
//! The directory holds:
//! - `N.lock`, held by the run in slot N for as long as its command runs.
//!   The slots that exist are the runs that may go at once, for every run
//!   alike; `--setup` creates and removes them;
//! - `N.holder`, the note that run writes when it starts. Its first line is
//!   for other runs to check: the pid, the word that tells that process
//!   from any other with the same pid, and the start in seconds since the
//!   Unix epoch. Its second line names the run for the runs that wait. A
//!   note counts only while that very process runs. It is a separate file
//!   because Windows locks are mandatory: while one process holds a lock,
//!   no other process can read the locked file;
//! - `gate.lock`, held while one run decides whether it may start or
//!   `--setup` changes the slots, so no two of them act at once. A run lists
//!   and opens the slots anew each time it holds the gate, so it never acts
//!   on slots that `--setup` has removed.
//!
//! A run starts at once when no other run is running. Beside other runs, it
//! needs a free slot, every running run to have recorded a start `ramp_up`
//! ago or more, and memory to spare.
//! A run that could not write its note, as in a sandbox, records no start,
//! so no run starts beside it.

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
}

impl Queue {
    /// The queue in `CARGO_QUEUE_DIR`, or `~/.cache/cargo-queue`.
    pub fn from_env() -> io::Result<Self> {
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
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Makes the queue have exactly the slots 1 to `slots`, so that many
    /// runs may go at once, and creates the files a sandboxed run cannot.
    /// Changes nothing while a slot it would remove is in use: a slot left
    /// behind would stay in use by later runs.
    pub fn set_up(&self, slots: NonZeroU32) -> io::Result<()> {
        let Some(_gate) = self.enter_gate()? else {
            return Err(io::Error::other(format!(
                "another run holds {}; try again",
                self.dir.join("gate.lock").display()
            )));
        };
        let surplus: Vec<u32> = self
            .slot_numbers()?
            .into_iter()
            .filter(|&number| number > slots.get())
            .collect();
        let mut in_use = Vec::new();
        for &number in &surplus {
            // Dropping the file releases the lock again; the gate keeps
            // other runs from taking the slot before it is removed.
            match self.open_slot(number)?.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => in_use.push(number.to_string()),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        if !in_use.is_empty() {
            return Err(io::Error::other(format!(
                "slot {} is in use, so nothing was changed; try again when its run has ended",
                in_use.join(" and slot ")
            )));
        }
        self.replace_slots(&surplus, slots.get()).map_err(|error| {
            let left = self.slot_numbers().map_or_else(
                |_| "unknown slots".to_owned(),
                |numbers| {
                    let numbers: Vec<_> = numbers.iter().map(u32::to_string).collect();
                    format!("slots {}", numbers.join(", "))
                },
            );
            io::Error::new(
                error.kind(),
                format!("{error}; {} now has {left}", self.dir.display()),
            )
        })
    }

    fn replace_slots(&self, surplus: &[u32], slots: u32) -> io::Result<()> {
        for &number in surplus {
            for path in [self.lock_path(number), self.holder_path(number)] {
                match fs::remove_file(&path) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => {
                        let message = format!("cannot remove {} ({error})", path.display());
                        return Err(io::Error::new(error.kind(), message));
                    }
                    _ => {}
                }
            }
        }
        (1..=slots).try_for_each(|number| self.create_slot(number).map(drop))
    }

    /// Waits until this run may start, telling the user whom it waits for,
    /// then notes `name` for the runs that queue behind this one.
    pub fn wait_for_turn(&self, name: &str) -> io::Result<Turn> {
        self.wait(name, &Timing::REAL, platform::memory, |message| {
            eprintln!("cargo-queue: {message}");
        })
    }

    /// Checks the queue every `timing.poll` until this run may start, and
    /// reports whom it waits for: at first, every `timing.report` after, and
    /// how long it took.
    fn wait(
        &self,
        name: &str,
        timing: &Timing,
        mut memory: impl FnMut() -> Memory,
        mut report: impl FnMut(String),
    ) -> io::Result<Turn> {
        let waiting = Instant::now();
        // The latest reason to wait; none yet while the gate stays busy.
        let mut blocked = None;
        let mut next_report = None;
        let turn = loop {
            match self.try_start(name, timing.ramp_up, &mut memory)? {
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

    /// Takes a slot and notes `name` in it if this run may start now.
    fn try_start(
        &self,
        name: &str,
        ramp_up: Duration,
        memory: &mut impl FnMut() -> Memory,
    ) -> io::Result<Attempt> {
        let Some(_gate) = self.enter_gate()? else {
            return Ok(Attempt::Busy);
        };
        let mut numbers = self.slot_numbers()?;
        if numbers.is_empty() {
            // A queue nobody has set up has one slot.
            self.create_slot(1)?;
            numbers.insert(1);
        }
        let Judgment { running, free } = self.judge(&numbers, ramp_up, memory)?;
        Ok(match free {
            // Written before the gate opens, so the next run to decide sees
            // when this one started.
            Ok((number, lock)) => Attempt::Started(Turn {
                holder: write_holder(&self.holder_path(number), name),
                _lock: lock,
            }),
            Err(reason) => Attempt::Blocked(Blocked {
                slots: running,
                reason,
            }),
        })
    }

    /// Finds the running runs and a slot a new run may take beside them
    /// now. Only for whoever holds the gate.
    fn judge(
        &self,
        numbers: &BTreeSet<u32>,
        ramp_up: Duration,
        memory: &mut impl FnMut() -> Memory,
    ) -> io::Result<Judgment> {
        let mut free = None;
        let mut running = Vec::new();
        for &number in numbers {
            let lock = self.open_slot(number)?;
            match lock.try_lock() {
                Ok(()) if free.is_none() => free = Some((number, lock)),
                // Dropping the file releases the lock again.
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => running.push(number),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        let reason = if free.is_none() {
            Some(Reason::Full)
        } else if running.is_empty() {
            None
        } else {
            let starts: Option<Vec<_>> = running
                .iter()
                .map(|&number| self.note(number).map(|note| note.started))
                .collect();
            match starts.and_then(|starts| starts.into_iter().max()) {
                None => Some(Reason::Unrecorded),
                // A start time ahead of the clock counts as just now.
                Some(latest)
                    if SystemTime::now().duration_since(latest).unwrap_or_default() < ramp_up =>
                {
                    Some(Reason::Starting)
                }
                Some(_) => match memory() {
                    Memory::Spare => None,
                    Memory::Tight => Some(Reason::Tight),
                    Memory::Unknown => Some(Reason::Unknown),
                },
            }
        };
        let free = match (reason, free) {
            (None, Some(free)) => Ok(free),
            (Some(reason), _) => Err(reason),
            (None, None) => unreachable!("a queue without a free slot is full"),
        };
        Ok(Judgment { running, free })
    }

    /// The queue as a new run would find it, for `--status`. Creates
    /// nothing and keeps no lock.
    pub fn status(&self) -> io::Result<String> {
        self.report(Timing::REAL.ramp_up, platform::memory())
    }

    fn report(&self, ramp_up: Duration, memory: Memory) -> io::Result<String> {
        let mut lines = vec![format!("queue: {}", self.dir.display())];
        let memory_line = match memory {
            Memory::Spare => "memory: to spare",
            Memory::Tight => "memory: under pressure",
            Memory::Unknown => "memory: unavailable",
        };
        let verdict = match self.inspect(&mut lines, ramp_up, memory)? {
            Inspection::NotSetUp => "would set the queue up with one slot and start".to_owned(),
            Inspection::Busy => "cannot be judged now: another run holds gate.lock".to_owned(),
            Inspection::Judged(None) => "would start".to_owned(),
            Inspection::Judged(Some(Reason::Full)) => "would wait for a free slot".to_owned(),
            Inspection::Judged(Some(reason)) => format!("would wait{}", reason.suffix()),
        };
        lines.extend([memory_line.to_owned(), format!("a new run {verdict}")]);
        Ok(lines.join("\n"))
    }

    /// Adds a line per slot to `lines` and judges a new run, as far as the
    /// queue can be read without creating anything.
    fn inspect(
        &self,
        lines: &mut Vec<String>,
        ramp_up: Duration,
        memory: Memory,
    ) -> io::Result<Inspection> {
        let path = self.dir.join("gate.lock");
        let gate = match File::open(&path) {
            Ok(gate) => gate,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Inspection::NotSetUp);
            }
            Err(error) => return Err(cannot_open(&path, error)),
        };
        let Some(_gate) = keep_gate(gate)? else {
            return Ok(Inspection::Busy);
        };
        let numbers = self.slot_numbers()?;
        if numbers.is_empty() {
            return Ok(Inspection::NotSetUp);
        }
        let Judgment { running, free } = self.judge(&numbers, ramp_up, &mut || memory)?;
        for number in numbers {
            let state = if !running.contains(&number) {
                "free".to_owned()
            } else if let Some(note) = self.note(number) {
                match SystemTime::now().duration_since(note.started) {
                    Ok(held) => format!("{} (held for {})", note.name, elapsed(held)),
                    Err(_) => note.name,
                }
            } else {
                "in use by a run that left no note".to_owned()
            };
            lines.push(format!("slot {number}: {state}"));
        }
        Ok(Inspection::Judged(free.err()))
    }

    /// Takes `gate.lock`, creating it if need be, unless another run keeps
    /// it for `GATE_PATIENCE`.
    fn enter_gate(&self) -> io::Result<Option<File>> {
        keep_gate(open_or_create(&self.dir.join("gate.lock"))?)
    }

    /// The note of the run in slot `number`, if it wrote one.
    fn note(&self, number: u32) -> Option<Note> {
        Note::read(&fs::read_to_string(self.holder_path(number)).ok()?)
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
                        .map_or_else(|| "another build".to_owned(), |note| note.name)
                })
                .collect(),
            None => self
                .slot_numbers()?
                .into_iter()
                .filter_map(|number| self.note(number))
                .map(|note| note.name)
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

    /// The slots that exist.
    fn slot_numbers(&self) -> io::Result<BTreeSet<u32>> {
        let mut numbers = BTreeSet::new();
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

    /// Opens the lock of a slot that exists; whoever holds the gate keeps
    /// `--setup` from removing it meanwhile.
    fn open_slot(&self, number: u32) -> io::Result<File> {
        let path = self.lock_path(number);
        File::open(&path).map_err(|error| cannot_open(&path, error))
    }

    fn create_slot(&self, number: u32) -> io::Result<File> {
        open_or_create(&self.lock_path(number))
    }

    fn lock_path(&self, number: u32) -> PathBuf {
        self.dir.join(format!("{number}.lock"))
    }

    fn holder_path(&self, number: u32) -> PathBuf {
        self.dir.join(format!("{number}.holder"))
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

/// A note that counts, as the process that wrote it runs.
struct Note {
    /// The run as waiting runs print it.
    name: String,
    /// When the run wrote it.
    started: SystemTime,
}

impl Note {
    /// The note this process leaves, unless the system will not say which
    /// process it is.
    fn write(name: &str) -> Option<String> {
        let pid = process::id();
        let incarnation = platform::incarnation(pid)?;
        let started = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
        Some(format!(
            "{pid} {incarnation} {}\n{name}\n",
            started.as_secs()
        ))
    }

    /// The note in `contents` if its writer still runs. Any other note was
    /// left by a run that ended without clearing it, and the run in the slot
    /// now could not write its own.
    fn read(contents: &str) -> Option<Self> {
        let (writer, name) = contents.split_once('\n')?;
        let mut writer = writer.split(' ');
        let pid = writer.next()?.parse().ok()?;
        let incarnation = writer.next()?;
        let started = UNIX_EPOCH.checked_add(Duration::from_secs(writer.next()?.parse().ok()?))?;
        (platform::incarnation(pid).as_deref() == Some(incarnation)).then(|| Self {
            name: name.trim_end().to_owned(),
            started,
        })
    }
}

/// The running runs' slots and, unless a new run must wait, the free slot
/// it may take with the lock on it.
struct Judgment {
    running: Vec<u32>,
    free: Result<(u32, File), Reason>,
}

/// How far `--status` could look.
enum Inspection {
    /// No run has created the queue's files yet.
    NotSetUp,
    /// Another run has kept `gate.lock` for too long.
    Busy,
    /// Why a new run would wait, if it would.
    Judged(Option<Reason>),
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

/// Locks the opened `gate.lock` unless another run keeps it for
/// `GATE_PATIENCE`.
fn keep_gate(gate: File) -> io::Result<Option<File>> {
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

/// A length of time as `--status` prints it, such as `45s`, `3m12s` or
/// `2h05m`.
fn elapsed(time: Duration) -> String {
    let seconds = time.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Opens a lock file read-only, which is all locking needs, so a sandbox
/// that denies writes to it still queues. Only a missing file needs writes.
fn open_or_create(path: &Path) -> io::Result<File> {
    match File::open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => create(path).map_err(|error| {
            let message = format!(
                "cannot create {} ({error}); run `cargo-queue --setup` once outside the sandbox",
                path.display()
            );
            io::Error::new(error.kind(), message)
        }),
        result => result.map_err(|error| cannot_open(path, error)),
    }
}

fn create(path: &Path) -> io::Result<File> {
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
fn write_holder(path: &Path, name: &str) -> Option<File> {
    let mut file = File::create(path).ok()?;
    file.write_all(Note::write(name)?.as_bytes()).ok()?;
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

    /// A queue nobody has set up.
    fn bare_queue(dir: &TempDir) -> Queue {
        Queue {
            dir: dir.path().join("queue"),
        }
    }

    fn queue(dir: &TempDir, slots: u32) -> Queue {
        let queue = bare_queue(dir);
        set_up(&queue, slots).expect("set up the queue");
        queue
    }

    fn set_up(queue: &Queue, slots: u32) -> io::Result<()> {
        queue.set_up(NonZeroU32::new(slots).expect("at least one slot"))
    }

    fn files(queue: &Queue) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(&queue.dir)
            .expect("list the queue")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 name")
            })
            .collect();
        names.sort();
        names
    }

    fn timing(ramp_up: Duration) -> Timing {
        Timing {
            poll: Duration::from_millis(1),
            report: Duration::from_millis(10),
            ramp_up,
        }
    }

    /// A run as cargo-queue names it in its note.
    fn name(command: &str) -> String {
        format!("pid {} in /work: {command}", process::id())
    }

    /// A run that has to start without waiting.
    fn start(queue: &Queue, command: &str) -> Turn {
        let report = |message| panic!("{command} had to wait: {message}");
        queue
            .wait(
                &name(command),
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
        let name = name(command);
        let run = thread::spawn(move || {
            queue.wait(
                &name,
                &timing(ramp_up),
                || memory,
                |message| {
                    sender.send(message).expect("collect the message");
                },
            )
        });
        Waiter {
            run,
            messages,
            deadline: Instant::now() + TIMEOUT,
        }
    }

    /// The name in slot `number`'s note, whoever wrote it.
    fn holder(queue: &Queue, number: u32) -> String {
        let note = fs::read_to_string(queue.holder_path(number)).expect("read the note");
        note.lines().nth(1).unwrap_or_default().to_owned()
    }

    /// Makes the run in slot `number` look as if it started long ago.
    fn age(queue: &Queue, number: u32) {
        let path = queue.holder_path(number);
        let note = fs::read_to_string(&path).expect("read the note");
        let (writer, name) = note.split_once('\n').expect("a note of two lines");
        let (process, _started) = writer.rsplit_once(' ').expect("a start");
        fs::write(path, format!("{process} 0\n{name}")).expect("backdate the note");
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
        /// When the test gives up, however many messages still arrive.
        deadline: Instant,
    }

    impl Waiter {
        fn message(&self) -> String {
            let left = self.deadline.saturating_duration_since(Instant::now());
            self.messages.recv_timeout(left).expect("a message")
        }

        /// Skips messages until one ends with `suffix`.
        fn until(&self, suffix: &str) {
            while !self.message().ends_with(suffix) {}
        }

        /// Waits for the turn, whatever the run printed.
        fn turn(self) -> Turn {
            while !self.run.is_finished() {
                assert!(Instant::now() < self.deadline, "the run did not start");
                thread::sleep(Duration::from_millis(1));
            }
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
        assert_eq!(waiter.message(), format!("waiting for {}", name("first")));
        let still = waiter.message();
        assert!(still.starts_with("still waiting after "), "{still}");
        assert!(
            still.ends_with(&format!("s for {}", name("first"))),
            "{still}"
        );

        let renamed = Note::write(&name("renamed")).expect("a note");
        fs::write(queue.holder_path(1), renamed).expect("change the note");
        waiter.until(&format!("s for {}", name("renamed")));
        drop(first);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 1), name("second"));
    }

    #[test]
    fn a_lone_run_starts_whatever_the_memory() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let mut unknown = || Memory::Unknown;
        let attempt = queue
            .try_start(&name("first"), FOREVER, &mut unknown)
            .expect("an attempt");
        assert!(matches!(attempt, Attempt::Started(_)));
        assert_eq!(holder(&queue, 1), name("first"));
    }

    #[test]
    fn a_run_starts_beside_another_only_with_memory_to_spare() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        let tight = wait(&queue, "second", Duration::ZERO, Memory::Tight);
        assert_eq!(
            tight.message(),
            format!("waiting for {} (memory is under pressure)", name("first"))
        );
        let unknown = wait(&queue, "third", Duration::ZERO, Memory::Unknown);
        assert_eq!(
            unknown.message(),
            format!(
                "waiting for {} (memory information is unavailable)",
                name("first")
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
        assert_eq!(holder(&queue, 2), name("fifth"));
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
                name("first")
            )
        );
        age(&queue, 1);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 2), name("second"));
    }

    /// A sandboxed run cannot write its note, so nothing tells when it
    /// started; even one that has run for long keeps others from starting.
    #[test]
    fn no_run_starts_beside_one_without_a_note() {
        let dir = queue_dir();
        let queue = queue(&dir, 2);
        let first = start(&queue, "first");
        fs::write(queue.holder_path(1), "").expect("clear the note");
        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(
            waiter.message(),
            "waiting for another build (another run did not record when it started)"
        );
        drop(first);
        let _second = waiter.reported_turn();
        assert_eq!(holder(&queue, 1), name("second"));
    }

    /// A note is only a file, so anything may be in it.
    #[test]
    fn a_note_that_cannot_be_read_counts_for_nothing() {
        let pid = process::id();
        let own = platform::incarnation(pid).expect("this process");
        assert!(Note::read(&format!("{pid} {own} 0\nname\n")).is_some());
        for broken in [
            String::new(),
            format!("{pid} {own} 0"),
            format!("{pid} {own}\nname\n"),
            format!("{pid} {own} soon\nname\n"),
            format!("{pid} {own} {}\nname\n", u64::MAX),
            format!("pid {pid} in /work: name\n"),
        ] {
            assert!(Note::read(&broken).is_none(), "{broken:?}");
        }
    }

    /// A run that ended without clearing its note leaves it for a sandboxed
    /// run that takes the slot next and cannot write its own; the note
    /// neither names that run nor records its start. Nor does it when a
    /// later process has the pid of the run that wrote it.
    #[test]
    fn a_note_from_a_run_that_ended_counts_for_nothing() {
        let own = platform::incarnation(process::id()).expect("this process");
        let stale = [
            format!("{} {own} 0\npid 1 in /work: gone\n", ended_pid()),
            format!("{} another-{own} 0\npid 1 in /work: gone\n", process::id()),
        ];
        for stale in stale {
            let dir = queue_dir();
            let queue = queue(&dir, 2);
            let first = start(&queue, "first");
            fs::write(queue.holder_path(1), stale).expect("leave a stale note");
            let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
            assert_eq!(
                waiter.message(),
                "waiting for another build (another run did not record when it started)"
            );
            drop(first);
            let _second = waiter.reported_turn();
        }
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
            format!("waiting for 2 runs: {}; {}", name("first"), name("second"))
        );
        drop(first);
        let _third = third.reported_turn();
        assert_eq!(holder(&queue, 1), name("third"));
    }

    #[test]
    fn a_queue_nobody_set_up_has_one_slot() {
        let dir = queue_dir();
        let queue = bare_queue(&dir);
        let first = start(&queue, "first");
        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(waiter.message(), format!("waiting for {}", name("first")));
        assert_eq!(files(&queue), ["1.holder", "1.lock", "gate.lock"]);
        drop(first);
        let _second = waiter.reported_turn();
    }

    #[test]
    fn setup_makes_the_slots_it_is_asked_for() {
        let dir = queue_dir();
        let queue = bare_queue(&dir);
        fs::create_dir(&queue.dir).expect("create the queue");
        fs::write(queue.lock_path(3), "").expect("leave a gap before slot 3");
        set_up(&queue, 4).expect("grow the queue");
        assert_eq!(
            files(&queue),
            ["1.lock", "2.lock", "3.lock", "4.lock", "gate.lock"]
        );

        let first = start(&queue, "first");
        drop(wait(&queue, "second", Duration::ZERO, Memory::Spare).turn());
        set_up(&queue, 1).expect("shrink the queue");
        assert_eq!(files(&queue), ["1.holder", "1.lock", "gate.lock"]);
        assert_eq!(holder(&queue, 1), name("first"));
        drop(first);
    }

    /// A slot left behind would keep letting a second run go.
    #[test]
    fn setup_changes_nothing_while_a_slot_it_would_remove_is_in_use() {
        let dir = queue_dir();
        let queue = queue(&dir, 3);
        let _first = start(&queue, "first");
        let second = wait(&queue, "second", Duration::ZERO, Memory::Spare).turn();
        let before = files(&queue);

        let error = set_up(&queue, 1).expect_err("slot 2 is in use");
        assert_eq!(
            error.to_string(),
            "slot 2 is in use, so nothing was changed; try again when its run has ended"
        );
        assert_eq!(files(&queue), before);
        drop(second);
        set_up(&queue, 1).expect("shrink the queue");
        assert_eq!(files(&queue), ["1.holder", "1.lock", "gate.lock"]);
    }

    #[test]
    fn status_tells_what_a_new_run_would_find() {
        let dir = queue_dir();
        let queue = bare_queue(&dir);
        let at = format!("queue: {}", queue.dir.display());
        let report = |memory| queue.report(Duration::ZERO, memory).expect("a report");
        assert_eq!(
            report(Memory::Unknown),
            format!(
                "{at}\nmemory: unavailable\n\
                 a new run would set the queue up with one slot and start"
            )
        );
        assert!(!queue.dir.exists(), "status creates nothing");

        set_up(&queue, 2).expect("set up the queue");
        assert_eq!(
            report(Memory::Tight),
            format!(
                "{at}\nslot 1: free\nslot 2: free\nmemory: under pressure\n\
                 a new run would start"
            )
        );

        let first = start(&queue, "first");
        age(&queue, 1);
        let held = format!("slot 1: {} (held for ", name("first"));
        let tight = report(Memory::Tight);
        let lines: Vec<_> = tight.lines().collect();
        assert!(lines[1].starts_with(&held), "{tight}");
        assert_eq!(
            lines[2..],
            [
                "slot 2: free",
                "memory: under pressure",
                "a new run would wait (memory is under pressure)"
            ]
        );
        assert!(report(Memory::Spare).ends_with("memory: to spare\na new run would start"));
        let before = files(&queue);
        // The run that follows finds what status told, and status left
        // nothing behind to stand in its way.
        let second = wait(&queue, "second", Duration::ZERO, Memory::Spare).turn();
        assert_eq!(before, ["1.holder", "1.lock", "2.lock", "gate.lock"]);
        assert!(report(Memory::Spare).ends_with("a new run would wait for a free slot"));

        fs::write(queue.holder_path(2), "").expect("clear the note");
        drop(first);
        assert!(report(Memory::Spare).ends_with(
            "slot 1: free\nslot 2: in use by a run that left no note\nmemory: to spare\n\
                 a new run would wait (another run did not record when it started)"
        ));
        drop(second);
    }

    #[test]
    fn status_does_not_guess_while_the_gate_is_busy() {
        let dir = queue_dir();
        let queue = queue(&dir, 1);
        let gate = open_or_create(&queue.dir.join("gate.lock")).expect("open the gate");
        gate.lock().expect("hold the gate");
        assert_eq!(
            queue
                .report(Duration::ZERO, Memory::Spare)
                .expect("a report"),
            format!(
                "queue: {}\nmemory: to spare\n\
                 a new run cannot be judged now: another run holds gate.lock",
                queue.dir.display()
            )
        );
    }

    #[test]
    fn elapsed_time_reads_in_its_largest_units() {
        let cases = [
            (0, "0s"),
            (59, "59s"),
            (192, "3m12s"),
            (3600, "1h00m"),
            (7500, "2h05m"),
        ];
        for (seconds, shown) in cases {
            assert_eq!(elapsed(Duration::from_secs(seconds)), shown);
        }
    }

    /// A run stopped while it decides keeps the gate; the others still say
    /// whom they wait for.
    #[test]
    fn a_busy_gate_does_not_silence_waiting_runs() {
        let dir = queue_dir();
        let queue = queue(&dir, 1);
        let first = start(&queue, "first");
        let gate = open_or_create(&queue.dir.join("gate.lock")).expect("open the gate");
        gate.lock().expect("hold the gate");

        let waiter = wait(&queue, "second", Duration::ZERO, Memory::Spare);
        assert_eq!(waiter.message(), format!("waiting for {}", name("first")));
        waiter.until(&format!("s for {}", name("first")));
        drop(first);
        waiter.until("s for another build");
        drop(gate);
        let _second = waiter.reported_turn();
    }
}
