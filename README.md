# cargo-queue

Run heavy Cargo commands one at a time across all the projects on a machine.

Several Rust builds at once can exhaust a machine's memory, for example when
coding agents in different projects each start `cargo test`. cargo-queue makes
those commands take turns: each waits for a machine-wide lock, runs, and
releases the lock when it exits.

```bash
cargo queue test -p some-crate            # any Cargo command
cargo-queue -- ./scripts/check.sh         # any command that runs Cargo
cargo-queue --background -- cargo build   # the same, at lower priority
```

## Installation

```bash
cargo install --locked --git https://github.com/hwtnb/cargo-queue
```

This installs `cargo-queue` into `~/.cargo/bin`, which also makes it
available as `cargo queue`. Cargo looks the subcommand up along `PATH`, so
another `cargo-queue` earlier on `PATH` takes precedence; `which -a
cargo-queue` shows the order.

Then, outside any sandbox, create the queue's files:

```bash
cargo-queue --setup
```

Runs inside a sandbox that cannot write to `~/.cache`, such as Codex's
`workspace-write` mode, need these files to exist already. It runs nothing.

A sandboxed run queues with those files alone, but it cannot write its note,
so waiting runs cannot name it and no other run starts beside it. To let it
write the note, make the queue directory writable in the sandbox. For Codex,
in `~/.codex/config.toml`:

```toml
[sandbox_workspace_write]
writable_roots = ["~/.cache/cargo-queue"]
```

To let several builds go at once while memory allows, give the queue that
many slots:

```bash
cargo-queue --setup 2
```

The slots belong to the queue, so every run on the machine uses them without
further settings. `cargo-queue --setup` without a number goes back to one
slot, which is also what a queue has before any setup. While a slot it would
remove is in use, it changes nothing and says so; run it again when that run
has ended.

## Usage

```text
cargo queue [--background] <cargo arguments...>
cargo-queue [--background] -- <command...>
cargo-queue --setup [<slots>] | --help | --version
```

The first form runs Cargo: `$CARGO`, which Cargo sets for its subcommands, or
else `cargo`. The second form runs any command, such as a script or task runner
that calls Cargo itself. cargo-queue reads its own options only before the
command; everything from the first other argument on is passed through
unchanged, so `cargo queue test --help` shows Cargo's help.

To use another toolchain, name it before `queue`, as in
`cargo +nightly queue build`. rustup reads `+toolchain` only as Cargo's first
argument, so cargo-queue rejects `cargo queue +nightly build` without waiting.

A command that has to wait says for whom, says so again every minute with
whoever holds the lock by then, and says when it starts:

```text
cargo-queue: waiting for pid 4242 in /home/me/other-project: cargo clippy
cargo-queue: still waiting after 60s for pid 4242 in /home/me/other-project: cargo clippy
cargo-queue: still waiting after 120s for pid 5150 in /home/me/third-project: cargo test
cargo-queue: started after 137s
```

When the queue has several slots, a waiting run lists every run it waits for, and says why when a slot is free but it still may not
start:

```text
cargo-queue: waiting for 2 runs: pid 4242 in /home/me/a: cargo clippy; pid 5150 in /home/me/b: cargo test
cargo-queue: waiting for pid 4242 in /home/me/a: cargo clippy (memory is under pressure)
cargo-queue: waiting for pid 4242 in /home/me/a: cargo clippy (memory information is unavailable)
cargo-queue: waiting for another build (another run did not record when it started)
cargo-queue: waiting for pid 4242 in /home/me/a: cargo clippy (another run is still starting)
```

Only commands started through cargo-queue take turns. To make coding agents use
it, say so in their instructions, for example: "Run Cargo build, check, clippy,
test and run commands through `cargo queue`, and scripts that call Cargo
through `cargo-queue -- <command>`."

### Options and environment

| Name | Effect |
|---|---|
| `--background` | Lowers the command's priority. On macOS it runs under `taskpolicy -b`, on efficiency cores with low I/O priority, which can make builds much slower. On Windows it uses the below-normal priority class. Other systems reject it. |
| `CARGO_QUEUE_DIR` | The directory that holds the queue's files. Defaults to `~/.cache/cargo-queue`. |
| `--setup [<slots>]` | Gives the queue that many slots, one without a number, and runs nothing. |
| `CARGO_QUEUE_HELD` | Set to `1` for the command. A cargo-queue started inside it runs at once instead of waiting for its own ancestor. |

### Exit status

| Status | Meaning |
|---|---|
| The command's own | The command exited normally. |
| 128 + N | Signal N ended the command (Unix). |
| 127 | The command was not found. |
| 126 | The command could not be started. |
| 2 | The arguments name no command, put a `+toolchain` after `queue`, give `--setup` something other than a number from 1 up, or ask for something this system does not support. |
| 1 | Another error, such as a lock file that cannot be created or a `--setup` that would remove a slot in use. |

## How it works

Each run that goes holds an exclusive lock on one of the numbered files
`1.lock`, `2.lock` and so on in the queue directory (`flock` on Unix,
`LockFileEx` on Windows), and keeps it until its command exits. It writes
`pid <PID> in <directory>: <command>` to the matching `1.holder`, `2.holder`
and so on, which is what waiting runs print; Cargo appears there as `cargo`,
whichever toolchain's binary runs. A note counts only while the
process it names is running; any other note was left by a run that ended
without clearing it. The note lives in its own file because a Windows lock
keeps other processes from reading the locked file.

A run starts at once when no other run is running. Beside other runs, it
starts only when all of the following hold:

- a slot is free;
- every running run recorded a start at least 30 seconds ago, since a
  build's memory use grows after it starts. A run's start is when it wrote
  its note. A run that could not write its note, as in a sandbox, records
  no start, so no run starts beside it;
- memory is to spare. On macOS, the memory pressure level is normal and at
  least 25% of memory is free. On Linux, at least 25% of memory is available
  and, where the kernel reports memory pressure, tasks waited for memory less
  than 10% of the last ten seconds. On Windows, at least 25% of physical
  memory is available. When the system does not tell, as macOS does not
  inside Codex's sandbox, the run does not start beside others.

A waiting run checks again four times a second. `gate.lock` keeps two runs
from deciding at once, and keeps `--setup` from changing the slots while one
decides. Runs that are already going are never paused.

On Unix, SIGINT, SIGTERM and SIGHUP are passed on to the command, and the lock
is released only after the command has exited. On Windows, Ctrl-C and
Ctrl-Break reach the command through the console, and a job object ends the
command if cargo-queue is terminated.

cargo-queue opens the lock files read-only, which is all locking needs. A
sandbox that denies writes to them, such as Codex's `workspace-write` mode,
can therefore still take part, as long as `cargo-queue --setup` or another
run outside the sandbox has created them. A holder that cannot write its note
shows as "another build".

## Limitations

- Editors, rust-analyzer and Cargo commands started without cargo-queue
  neither wait nor make others wait.
- Waiting runs start in no particular order; the operating system's lock
  does not promise first come, first served.
- A run stopped with Ctrl-Z at the moment it holds `gate.lock` keeps every
  other run from starting until it resumes. The others keep reporting that
  they wait.
- A note left by a run that was killed counts again if another process
  later gets the same pid: waiting runs print it, and its old start counts
  for the run now in the slot.
- The memory check sees only the moment a run starts. Builds that grow
  later can still crowd memory, and running ones are not paused when they do.
- A command stopped with Ctrl-Z keeps the lock until it resumes or ends.
- On Unix, if cargo-queue itself is killed with SIGKILL, the command keeps
  running without holding the lock.

## Development

Run the checks with `cargo fmt --check`, `cargo clippy --all-targets -- -D
warnings` and `cargo test`; CI runs them on Linux, macOS and Windows. The
tests use a private queue directory and never touch the real one.
[AGENTS.md](AGENTS.md) is the contributor guide, written for coding agents and
people alike.

## License

MIT; see [LICENSE](LICENSE).
