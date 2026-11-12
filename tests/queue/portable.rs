//! Behavior that is the same on every system.

use std::fs;

use libtest_mimic::Trial;

use crate::child::ROLE;
use crate::os;
use crate::support::{Fixture, ReadOnly, Run, child_exe};

pub fn trials() -> Vec<Trial> {
    trials![
        runs_wait_for_the_holder_and_name_it,
        the_holder_note_is_cleared_afterwards,
        a_nested_run_does_not_wait,
        the_command_sees_the_lock_as_held,
        exit_codes_follow_the_command_and_shell_conventions,
        processes_left_behind_by_the_command_do_not_hold_the_lock,
        read_only_lock_and_holder_files_still_queue,
        the_cargo_form_runs_cargo_from_the_environment,
        the_note_names_cargo_without_its_path,
        the_command_form_skips_the_subcommand_name,
        help_version_and_usage_errors,
        an_invalid_maximum_is_a_usage_error,
        setup_creates_every_file_and_runs_nothing,
    ]
}

fn runs_wait_for_the_holder_and_name_it() {
    let fixture = Fixture::new();
    let log = fixture.path("log");
    let log = log.to_str().expect("a UTF-8 path");
    let (mut holder, held) = fixture.hold(&[log, "first"]);

    let mut waiter = fixture.start_child("append", &[log, "second"]);
    let expected = format!(
        "cargo-queue: waiting for pid {} in {}: {} {log} first",
        holder.pid(),
        held.cwd,
        child_exe()
    );
    assert_eq!(waiter.stderr_line(), expected);

    holder.release();
    assert!(holder.finish().status.success());
    let waited = waiter.finish();
    assert!(waited.status.success());
    assert!(
        waited.stderr[0].starts_with("cargo-queue: started after "),
        "{:?}",
        waited.stderr
    );
    assert_eq!(fixture.read("log"), "first\nsecond\n");
}

fn the_holder_note_is_cleared_afterwards() {
    let fixture = Fixture::new();
    let holder = fixture.holder();
    let holder = holder.to_str().expect("a UTF-8 path");
    let mut run = fixture.start_child("print-file", &[holder]);
    let note = run.stdout_line();
    let pid = run.pid();
    assert!(run.finish().status.success());
    assert!(note.starts_with(&format!("pid {pid} in ")), "{note}");
    assert!(
        note.ends_with(&format!(": {} {holder}", child_exe())),
        "{note}"
    );
    assert_eq!(
        fs::read_to_string(fixture.holder()).expect("read the note"),
        ""
    );
    assert_eq!(
        fs::read_to_string(fixture.lock()).expect("read the lock"),
        ""
    );
}

fn a_nested_run_does_not_wait() {
    let fixture = Fixture::new();
    let (holder, _) = fixture.hold(&[]);
    let mut nested = fixture.child("print-args", &["nested"]);
    nested.env("CARGO_QUEUE_HELD", "1");
    let nested = Run::start(nested).finish();
    assert!(nested.status.success());
    assert_eq!(nested.stdout, ["nested"]);
    assert!(nested.stderr.is_empty(), "{:?}", nested.stderr);
    assert!(holder.finish().status.success());
}

fn the_command_sees_the_lock_as_held() {
    let fixture = Fixture::new();
    let run = fixture
        .start_child("print-env", &["CARGO_QUEUE_HELD"])
        .finish();
    assert_eq!(run.stdout, ["1"]);
}

fn exit_codes_follow_the_command_and_shell_conventions() {
    let fixture = Fixture::new();
    let run = fixture.start_child("exit", &["7"]).finish();
    assert_eq!(run.status.code(), Some(7), "{:?}", run.stderr);

    let missing = fixture
        .start(&["--", "cargo-queue-test-no-such-command"])
        .finish();
    assert_eq!(missing.status.code(), Some(127));
    assert!(
        missing.stderr[0].starts_with("cargo-queue: cannot run cargo-queue-test-no-such-command: "),
        "{:?}",
        missing.stderr
    );

    // Neither a Unix executable nor a Windows program.
    fs::write(fixture.path("not-a-program.exe"), "plain text").expect("write a plain file");
    let unstartable = fixture.start(&["--", "./not-a-program.exe"]).finish();
    assert_eq!(
        unstartable.status.code(),
        Some(126),
        "{:?}",
        unstartable.stderr
    );
}

fn processes_left_behind_by_the_command_do_not_hold_the_lock() {
    let fixture = Fixture::new();
    let run = fixture.start_child("leave-behind", &[]).finish();
    let sleeper = run.stdout[0].parse().expect("the sleeper's pid");
    let next = fixture.start_child("exit", &["0"]).finish();
    os::kill(sleeper);
    assert!(next.status.success());
    assert!(next.stderr.is_empty(), "{:?}", next.stderr);
}

/// A sandbox may deny writes to every file in the queue; the locks still
/// work.
fn read_only_lock_and_holder_files_still_queue() {
    let fixture = Fixture::new();
    let _files: Vec<_> = ["gate.lock", "1.lock", "1.holder"]
        .into_iter()
        .map(|name| {
            let path = fixture.queue().join(name);
            fs::write(&path, "").expect("create the file");
            ReadOnly::new(path)
        })
        .collect();

    let (holder, _) = fixture.hold(&[]);
    let mut waiter = fixture.start_child("exit", &["0"]);
    assert_eq!(
        waiter.stderr_line(),
        "cargo-queue: waiting for another build"
    );
    assert!(holder.finish().status.success());
    assert!(waiter.finish().status.success());
}

fn the_cargo_form_runs_cargo_from_the_environment() {
    let fixture = Fixture::new();
    let mut command = fixture.cargo_queue(&["queue", "test", "--release", "--", "--nocapture"]);
    command.env("CARGO", child_exe()).env(ROLE, "print-args");
    let run = Run::start(command).finish();
    assert!(run.status.success(), "{:?}", run.stderr);
    assert_eq!(run.stdout, ["test --release -- --nocapture"]);
}

fn the_note_names_cargo_without_its_path() {
    let fixture = Fixture::new();
    let holder = fixture.holder();
    let holder = holder.to_str().expect("a UTF-8 path");
    let mut command = fixture.cargo_queue(&["queue", holder]);
    command.env("CARGO", child_exe()).env(ROLE, "print-file");
    let run = Run::start(command).finish();
    assert!(run.status.success(), "{:?}", run.stderr);
    let suffix = format!(": cargo {holder}");
    assert!(run.stdout[0].ends_with(&suffix), "{:?}", run.stdout);
}

fn the_command_form_skips_the_subcommand_name() {
    let fixture = Fixture::new();
    let exe = child_exe();
    let mut command = fixture.cargo_queue(&["queue", "--", &exe, "ran"]);
    command.env(ROLE, "print-args");
    let run = Run::start(command).finish();
    assert_eq!(run.stdout, ["ran"]);
}

fn help_version_and_usage_errors() {
    let fixture = Fixture::new();
    let help = fixture.start(&["queue", "--help"]).finish();
    assert!(help.status.success());
    assert!(help.stdout.iter().any(|line| line == "Usage:"));

    let version = fixture.start(&["-V"]).finish();
    assert_eq!(
        version.stdout,
        [format!("cargo-queue {}", env!("CARGO_PKG_VERSION"))]
    );

    for args in [&[][..], &["queue"], &["--background", "--"]] {
        let usage = fixture.start(args).finish();
        assert_eq!(usage.status.code(), Some(2), "{args:?}");
        assert!(usage.stderr.iter().any(|line| line == "Usage:"), "{args:?}");
    }
    let toolchain = fixture.start(&["queue", "+nightly", "build"]).finish();
    assert_eq!(toolchain.status.code(), Some(2));
    assert_eq!(
        toolchain.stderr,
        ["cargo-queue: write `cargo +nightly queue ...`, not `cargo queue +nightly ...`"]
    );
    assert!(
        !fixture.lock().exists(),
        "no lock is taken without a command"
    );
}

fn an_invalid_maximum_is_a_usage_error() {
    let fixture = Fixture::new();
    for max in ["0", "two", "-1"] {
        let mut command = fixture.child("exit", &["0"]);
        command.env("CARGO_QUEUE_MAX_RUNS", max);
        let run = Run::start(command).finish();
        assert_eq!(run.status.code(), Some(2), "{max}");
        assert_eq!(
            run.stderr,
            [format!(
                "cargo-queue: CARGO_QUEUE_MAX_RUNS must be a positive whole number, not {max}"
            )]
        );
    }
}

fn setup_creates_every_file_and_runs_nothing() {
    let fixture = Fixture::new();
    let queue = fixture.queue();
    for _ in 0..2 {
        let mut command = fixture.cargo_queue(&["queue", "--setup"]);
        command.env("CARGO_QUEUE_MAX_RUNS", "3");
        let run = Run::start(command).finish();
        assert!(run.status.success(), "{:?}", run.stderr);
        assert_eq!(
            run.stdout,
            [format!(
                "cargo-queue: created {} with 3 slots",
                queue.display()
            )]
        );
    }
    let mut files: Vec<_> = fs::read_dir(&queue)
        .expect("list the queue")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    files.sort();
    assert_eq!(files, ["1.lock", "2.lock", "3.lock", "gate.lock"]);
}
