//! Windows: Ctrl-C and Ctrl-Break reach every process on the console, the
//! command included, so cargo-queue only has to outlive them. A job object
//! ends the command if cargo-queue itself is terminated, so the command never
//! runs on after the lock is released.

use std::ffi::OsString;
use std::io;
use std::process::ExitStatus;
use std::ptr;

use tokio::process::{Child, Command};
use tokio::signal::windows::{CtrlBreak, CtrlC, ctrl_break, ctrl_c};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, WAIT_TIMEOUT};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetProcessTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use crate::failure::Failure;
use crate::platform::Memory;

pub struct Supervision {
    ctrl_c: CtrlC,
    ctrl_break: CtrlBreak,
}

/// The console already delivered the event to the command.
pub type Interruption = ();

impl Supervision {
    pub fn start() -> io::Result<Self> {
        if let Err(error) = end_children_with_this_process() {
            // Cargo treats its own job object as best effort too.
            eprintln!("cargo-queue: warning: the command may outlive cargo-queue ({error})");
        }
        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
        })
    }

    pub async fn next_interruption(&mut self) -> Interruption {
        tokio::select! {
            _ = self.ctrl_c.recv() => {}
            _ = self.ctrl_break.recv() => {}
        }
    }

    pub fn forward(&self, _child: &Child, _event: Interruption) {}
}

/// Puts cargo-queue in a job that ends all its processes when the last
/// handle to it closes. Children join the job when they start, as with
/// Cargo's own job object. The handle is never closed on purpose: Windows
/// closes it when cargo-queue exits, however it exits.
fn end_children_with_this_process() -> io::Result<()> {
    // SAFETY: null attributes and name are documented as valid and create an
    // unnamed job with default security.
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let size =
        u32::try_from(size_of_val(&limits)).expect("the limit structure is a few hundred bytes");
    // SAFETY: `job` is a valid job handle, and the pointer and size describe
    // `limits`, the structure this information class expects.
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles are valid; the current-process pseudo handle needs
    // no closing.
    if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .expect("a Windows process always has an exit code")
}

/// Runs the program at below-normal priority.
pub fn background(program: OsString) -> Result<Command, Failure> {
    let mut command = Command::new(program);
    command.creation_flags(BELOW_NORMAL_PRIORITY_CLASS);
    Ok(command)
}

/// When the process was created, in the system's 100-nanosecond units.
/// `None` once it has exited, even while handles keep it around, and when
/// it cannot be opened.
pub fn incarnation(pid: u32) -> Option<String> {
    let access = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: OpenProcess takes plain values and returns null on failure.
    let process = unsafe { OpenProcess(access, 0, pid) };
    if process.is_null() {
        return None;
    }
    // SAFETY: `process` is a valid handle with synchronize access; a zero
    // timeout only reads whether the process has exited.
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    let mut created = FILETIME::default();
    let mut unused = [FILETIME::default(); 3];
    // SAFETY: `process` is a valid handle with query access, and the four
    // pointers are to writable FILETIMEs.
    let read = unsafe {
        GetProcessTimes(
            process,
            &raw mut created,
            &raw mut unused[0],
            &raw mut unused[1],
            &raw mut unused[2],
        )
    } != 0;
    // SAFETY: `process` is a valid handle that is not used again.
    unsafe { CloseHandle(process) };
    (running && read).then(|| {
        let created = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
        created.to_string()
    })
}

/// Spare when at least a quarter of physical memory is available.
pub fn memory() -> Memory {
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).expect("a structure of 64 bytes"),
        ..MEMORYSTATUSEX::default()
    };
    // SAFETY: `status` is a writable MEMORYSTATUSEX whose `dwLength` is set,
    // as the function requires.
    if unsafe { GlobalMemoryStatusEx(&raw mut status) } == 0 {
        return Memory::Unknown;
    }
    Memory::judge(status.ullAvailPhys, status.ullTotalPhys)
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::process::{Command, Stdio};

    use super::*;

    /// `Child` keeps its handle after `wait`, so the process object stays.
    #[test]
    fn a_process_that_exited_has_no_incarnation_while_a_handle_keeps_it() {
        let mut child = Command::new(env::current_exe().expect("this executable"))
            .arg("--list")
            .stdout(Stdio::null())
            .spawn()
            .expect("start a process");
        child.wait().expect("wait for it");
        assert_eq!(incarnation(child.id()), None);
    }
}
