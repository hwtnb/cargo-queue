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

## Usage

```text
cargo queue [--background] <cargo arguments...>
cargo-queue [--background] -- <command...>
cargo-queue --help | --version
```

The first form runs Cargo: `$CARGO`, which Cargo sets for its subcommands, or
else `cargo`. The second form runs any command, such as a script or task runner
that calls Cargo itself. cargo-queue reads its own options only before the
command; everything from the first other argument on is passed through
unchanged, so `cargo queue test --help` shows Cargo's help.

A command that has to wait says for whom, says so again every minute with
whoever holds the lock by then, and says when it starts:

```text
cargo-queue: waiting for pid 4242 in /home/me/other-project: cargo clippy
cargo-queue: still waiting after 60s for pid 4242 in /home/me/other-project: cargo clippy
cargo-queue: still waiting after 120s for pid 5150 in /home/me/third-project: cargo test
cargo-queue: started after 137s
```

Only commands started through cargo-queue take turns. To make coding agents use
it, say so in their instructions, for example: "Run Cargo build, check, clippy,
test and run commands through `cargo queue`, and scripts that call Cargo
through `cargo-queue -- <command>`."

### Options and environment

| Name | Effect |
|---|---|
| `--background` | Lowers the command's priority. On macOS it runs under `taskpolicy -b`, on efficiency cores with low I/O priority, which can make builds much slower. On Windows it uses the below-normal priority class. Other systems reject it. |
| `CARGO_QUEUE_LOCK` | The lock file. Defaults to `~/.cache/cargo-queue.lock`. |
| `CARGO_QUEUE_HELD` | Set to `1` for the command. A cargo-queue started inside it runs at once instead of waiting for its own ancestor. |

### Exit status

| Status | Meaning |
|---|---|
| The command's own | The command exited normally. |
| 128 + N | Signal N ended the command (Unix). |
| 127 | The command was not found. |
| 126 | The command could not be started. |
| 2 | The arguments name no command, or ask for something this system does not support. |
| 1 | Another error, such as a lock file that cannot be created. |

## How it works

cargo-queue takes an exclusive lock on the lock file (`flock` on Unix,
`LockFileEx` on Windows) and keeps it until the command exits. The run that
holds the lock writes `pid <PID> in <directory>: <command>` to
`<lock file>.holder`, which is what waiting runs print. The note lives in its
own file because a Windows lock keeps other processes from reading the locked
file.

On Unix, SIGINT, SIGTERM and SIGHUP are passed on to the command, and the lock
is released only after the command has exited. On Windows, Ctrl-C and
Ctrl-Break reach the command through the console, and a job object ends the
command if cargo-queue is terminated.

cargo-queue opens the lock file read-only, which is all locking needs. A
sandbox that denies writes to it, such as Codex's `workspace-write` mode, can
therefore still take part, as long as some run outside the sandbox has created
the file. A holder that cannot write its note shows as "another build".

## Limitations

- Editors, rust-analyzer and Cargo commands started without cargo-queue
  neither wait nor make others wait.
- Waiting runs start in no particular order; the operating system's lock
  does not promise first come, first served.
- A command stopped with Ctrl-Z keeps the lock until it resumes or ends.
- On Unix, if cargo-queue itself is killed with SIGKILL, the command keeps
  running without holding the lock.

## Development

Run the checks with `cargo fmt --check`, `cargo clippy --all-targets -- -D
warnings` and `cargo test`; CI runs them on Linux, macOS and Windows. The
tests use a private lock file and never touch the real one.
[AGENTS.md](AGENTS.md) is the contributor guide, written for coding agents and
people alike.

## License

MIT; see [LICENSE](LICENSE).
