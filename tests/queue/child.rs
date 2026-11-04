//! The roles this test executable plays as the command cargo-queue runs.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::Duration;

/// Names the role; the arguments are the role's own.
pub const ROLE: &str = "CARGO_QUEUE_TEST_ROLE";

/// Plays the requested role and exits, or returns when no role is set.
pub fn run_if_requested() {
    let Some(role) = env::var_os(ROLE) else {
        return;
    };
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match role.to_str().expect("a UTF-8 role") {
        "hold" => hold(&args),
        "append" => {
            append(&args[0], &args[1]);
            0
        }
        "exit" => args[0].parse().expect("an exit code"),
        "print-args" => {
            println!("{}", args.join(" "));
            0
        }
        "print-env" => {
            println!("{}", env::var(&args[0]).unwrap_or_default());
            0
        }
        "print-file" => {
            print!("{}", fs::read_to_string(&args[0]).expect("read the file"));
            0
        }
        "leave-behind" => leave_behind(),
        "sleep" => {
            thread::sleep(Duration::from_secs(30));
            0
        }
        #[cfg(windows)]
        "print-priority" => crate::os::print_priority(),
        role => panic!("unknown role {role}"),
    };
    process::exit(code)
}

/// Prints its pid and working directory, waits for stdin to close, then
/// appends `entry` to `log` if they are given.
fn hold(args: &[String]) -> i32 {
    println!("{}", process::id());
    println!(
        "{}",
        env::current_dir().expect("a working directory").display()
    );
    io::stdin()
        .read_to_end(&mut Vec::new())
        .expect("read stdin");
    if let [log, entry] = args {
        append(log, entry);
    }
    0
}

fn append(log: &str, entry: &str) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("open the log");
    writeln!(log, "{entry}").expect("append to the log");
}

/// Starts a process that outlives this one and prints its pid.
#[expect(
    clippy::zombie_processes,
    reason = "the sleeper must outlive this process"
)]
fn leave_behind() -> i32 {
    let sleeper = Command::new(env::current_exe().expect("this executable"))
        .env(ROLE, "sleep")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the sleeper");
    println!("{}", sleeper.id());
    0
}
