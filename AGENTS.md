# Agent Operating Guide

cargo-queue is a small Rust wrapper that runs heavy Cargo commands one at a
time across the projects on a machine. [README](README.md) is for people who
install and use it; this file is contributor guidance. `CLAUDE.md` is a
symlink to this file.

## Contracts to keep

Agents on the machine call this tool from their own instructions, so these
are interfaces. Change them only with agreement on concrete before/after
examples, and update README with them.

- Invocation: `cargo queue [--background] <cargo arguments...>` and
  `cargo-queue [--background] -- <command...>`, and
  `cargo-queue --setup [<slots>]` to set how many runs may go at once, one
  without a number. Only leading options belong
  to cargo-queue; everything from the first other argument on reaches Cargo
  unchanged.
- Environment: `CARGO_QUEUE_DIR` names the queue directory,
  `CARGO_QUEUE_HELD=1` marks a nested call, and `CARGO` names the Cargo to
  run. Everything else
  passes through to the command unchanged. On macOS, SIP removes `DYLD_*`
  variables when a protected binary such as `/usr/bin/env` or `/bin/sh`
  starts; never start the command through one, or a directly invoked `$CARGO`
  loses rustup's library path.
- Files: the queue directory holds `N.lock` per slot and `gate.lock`. The
  slots that exist are the runs that may go at once; only `--setup` removes
  them, and a run creates slot 1 when there is none. Lock files are only
  opened and locked, never written. The holder note lives in `N.holder` and
  has two lines: `<PID> <incarnation> <start in Unix seconds>`, then
  `pid <PID> in <cwd>: <command>`, which waiting runs print. It counts only
  while the process with that pid and incarnation runs.
- Messages: the "waiting for", "still waiting after" and "started after"
  lines on stderr, as README shows them.
- Exit codes: the command's own; 128+N for signal N; 127 when the command is
  not found; 126 when it cannot be started; 2 for usage errors; 1 otherwise.

## Code layout

- `src/main.rs` only declares the modules and exits with the code from
  `src/app.rs`, which carries out one invocation. It exits through
  `process::exit` because `ExitCode` cannot carry Windows' 32-bit exit codes,
  so `app::run` must drop every guard before it returns.
- `src/cli.rs` parses arguments into an `Invocation` without side effects.
  `src/queue.rs` owns the lock and the holder note. `src/child.rs` builds the
  command, supervises it and maps how it ended to an exit code. The
  queue's rules are unit-tested in `queue.rs` with injected timing and
  memory, since real memory is not under a test's control.
- `src/platform.rs` re-exports the names that `src/platform/unix.rs` and
  `src/platform/windows.rs` both define: signal or console handling,
  forwarding, the Windows job object, `--background`, exit status, which
  process has a pid and whether memory is to spare. Keep
  `cfg` out of the shared modules; extend that common surface instead.
- Every new crate needs a stated reason. Write dependency versions as
  `major.minor` (for example `"1.54"`); `Cargo.lock` is tracked. The package
  sets no `rust-version`.

## Building and verification

- Run Cargo build, check, clippy, test and install through `cargo queue`, and
  read memory pressure and swap first; use one or two jobs when memory is
  tight. `cargo fmt` needs no queue. A queued run may wait for another
  project's build.
- Before a commit, run `cargo fmt --check`, `cargo queue test`, and
  `cargo queue clippy --all-targets -- -D warnings` for the host and for
  `--target x86_64-pc-windows-msvc` and `--target x86_64-unknown-linux-musl`.
  `.github/workflows/ci.yml` runs the same checks natively on Linux, macOS
  and Windows; change both together.
- Integration tests in `tests/queue/` must run on Windows as well as Unix.
  The commands cargo-queue runs there are the test executable itself in a
  role from `child.rs`, which is why the target uses a `libtest-mimic`
  harness. Put shared behavior in `portable.rs` and system-specific behavior
  in `unix.rs` or `windows.rs`; only Unix signal tests script commands with
  `sh`.
- Tests run the built binary with a private `CARGO_QUEUE_DIR` and remove
  `CARGO_QUEUE_HELD`, which `cargo queue test` sets. Order events through
  pipes and the "waiting for" line, never sleeps; the first launch of a fresh
  binary can take most of a second.
- Never point a development build at the real queue directory. Check sandbox
  behavior with `codex sandbox -c 'sandbox_mode="workspace-write"' --
  <command>`; plain `codex sandbox` is read-only and does not match Codex.

## Commits and documents

- Use Conventional Commits: a lowercase type, an optional scope, and an
  imperative sentence-case subject without a final period. The body explains
  why in prose wrapped at 72 columns.
- Write repository documents in English. Keep agent handoffs, personal notes
  and machine-specific absolute paths out of tracked files.
- README describes the tool as published: what it does, how to install and
  use it, how it behaves and where it stops. Keep development history,
  rejected ideas and working notes out of it.
- Do not push; the maintainer pushes.
