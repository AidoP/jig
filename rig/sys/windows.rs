//! The system on Windows: console control events, process groups with job objects, named semaphores and the size of a
//! console.
//!
//! - An interrupt is a console control event, which a handler catches, as Ctrl-C, Ctrl-Break or closing the console.
//! - A command leads a process group of its own, which a Ctrl-Break event interrupts, and is put in a job object
//!   once it has started, which ends it and every process it started when it is killed.
//! - A jobserver is a named semaphore, whose count is its tokens, as GNU make has it on Windows.
//! - Files have no modes which make them executable, and there are no reflinks.
//! - A shell command line, as a ninja rule's command, is not run, as an edge runs a program, as each which jig writes
//!   does, so that rig needs no shell.

use std::ffi::{c_int, c_void};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

type Handle = *mut c_void;
type Bool = i32;

#[repr(C)]
#[derive(Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Default)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

/// What `GetConsoleScreenBufferInfo` gives of a console.
#[repr(C)]
#[derive(Default)]
struct ScreenBufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: SmallRect,
    maximum: Coord,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> Bool>, add: Bool) -> Bool;
    fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> Bool;
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
    fn TerminateJobObject(job: Handle, code: u32) -> Bool;
    fn OpenProcess(access: u32, inherit: Bool, pid: u32) -> Handle;
    fn CloseHandle(handle: Handle) -> Bool;
    fn CreateSemaphoreW(attributes: *mut c_void, initial: i32, maximum: i32, name: *const u16) -> Handle;
    fn OpenSemaphoreW(access: u32, inherit: Bool, name: *const u16) -> Handle;
    fn ReleaseSemaphore(semaphore: Handle, count: i32, previous: *mut i32) -> Bool;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn GetStdHandle(which: u32) -> Handle;
    fn GetConsoleScreenBufferInfo(console: Handle, info: *mut ScreenBufferInfo) -> Bool;
}

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_SET_QUOTA: u32 = 0x0100;
const SYNCHRONIZE: u32 = 0x0010_0000;
const SEMAPHORE_MODIFY_STATE: u32 = 0x0002;
const WAIT_OBJECT_0: u32 = 0;
/// `(DWORD)-11` and `(DWORD)-12`.
const STD_OUTPUT_HANDLE: u32 = 0xffff_fff5;
const STD_ERROR_HANDLE: u32 = 0xffff_fff4;
const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;

/// The signals rig sends and catches, as numbers, as they are on Unix, which stand for the console control events and
/// the ending of a job.
pub const SIGHUP: c_int = 1;
pub const SIGINT: c_int = 2;
pub const SIGKILL: c_int = 9;
pub const SIGTERM: c_int = 15;

/// Returns a string as Windows takes one, in UTF-16, ending with NUL.
fn wide(string: &str) -> Vec<u16> {
    string.encode_utf16().chain([0]).collect()
}

/// Returns the width of the console a file descriptor writes to, stdout's or stderr's, in columns, or `None` if it is
/// not a console.
pub fn terminal_width(fd: c_int) -> Option<usize> {
    let which = match fd {
        1 => STD_OUTPUT_HANDLE,
        2 => STD_ERROR_HANDLE,
        _ => return None,
    };
    // SAFETY: the standard handles are the process's own, which are not closed here
    let console = unsafe { GetStdHandle(which) };
    if console.is_null() || console == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = ScreenBufferInfo::default();
    // SAFETY: the call writes the information, which `info` has room for, and fails for what is not a console
    if unsafe { GetConsoleScreenBufferInfo(console, &raw mut info) } == 0 {
        return None;
    }
    let width = i32::from(info.window.right) - i32::from(info.window.left) + 1;
    usize::try_from(width).ok().filter(|&width| width > 0)
}

/// The signal which interrupted a build, or `0`.
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

unsafe extern "system" fn on_event(event: u32) -> Bool {
    let signal = match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => SIGINT,
        CTRL_CLOSE_EVENT => SIGHUP,
        _ => SIGTERM,
    };
    INTERRUPTED.store(signal, Ordering::SeqCst);
    // handled, so that the process is not ended, while the build stops what it runs
    1
}

/// Catches the console control events which interrupt a build while it is held, rather than letting them end the
/// process, so that the build can stop the commands it is running and wait for them. The handler is removed when it
/// is dropped.
pub struct Interrupts {
    installed: bool,
}
impl Interrupts {
    #[must_use]
    pub fn catch() -> Self {
        INTERRUPTED.store(0, Ordering::SeqCst);
        // SAFETY: the handler only stores to an atomic
        let installed = unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } != 0;
        Self { installed }
    }
    /// Returns the signal which interrupted the build since it started catching them, if one has, which none can if
    /// the handler could not be installed.
    #[must_use]
    pub fn interrupted(&self) -> Option<c_int> {
        if !self.installed {
            return None;
        }
        match INTERRUPTED.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }
}
impl Drop for Interrupts {
    fn drop(&mut self) {
        if self.installed {
            // SAFETY: the handler is the one which was added
            unsafe { SetConsoleCtrlHandler(Some(on_event), 0) };
        }
    }
}

/// The job object of each running command, by the id of the process which leads its group, as an address, since a
/// handle is a pointer, which is not `Send`.
static JOBS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

/// Have a command lead a process group of its own, which a Ctrl-Break event interrupts, as the build's console's
/// Ctrl-C does not reach it.
pub fn new_group(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// A process which leads a group has started, which is put in a job object of its own, so that it and what it starts
/// can be ended together. What it starts before it is put in the job is not in it.
pub fn group_started(leader: u32) {
    // SAFETY: a job without attributes or a name, and the process, which is opened to be assigned, then closed
    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, leader);
        if process.is_null() || AssignProcessToJobObject(job, process) == 0 {
            if !process.is_null() {
                CloseHandle(process);
            }
            CloseHandle(job);
            return;
        }
        CloseHandle(process);
        if let Ok(mut jobs) = JOBS.lock() {
            jobs.push((leader, job as usize));
        }
    }
}

/// A process which led a group has ended, whose job object is closed, leaving what it started which still runs.
pub fn group_ended(leader: u32) {
    let job = JOBS.lock().ok().and_then(|mut jobs| {
        let index = jobs.iter().position(|&(pid, _)| pid == leader)?;
        Some(jobs.remove(index).1)
    });
    if let Some(job) = job {
        // SAFETY: the handle was the job's, which is closed once, as it was taken from the list
        unsafe { CloseHandle(job as Handle) };
    }
}

/// Send a signal to the process group of a process, which leads it: `SIGKILL` ends its job, and any other is a
/// Ctrl-Break event, or, where there is no console to send one on, also ends its job.
pub fn signal_group(leader: u32, signum: c_int) {
    let end = || {
        let jobs = JOBS.lock().ok();
        let job = jobs.as_ref().and_then(|jobs| jobs.iter().find(|&&(pid, _)| pid == leader).map(|&(_, job)| job));
        if let Some(job) = job {
            // SAFETY: the job is still in the list, which is locked, so its handle is open
            unsafe { TerminateJobObject(job as Handle, 1) };
        }
    };
    if signum == SIGKILL {
        end();
        return;
    }
    // SAFETY: the event goes to the process group, which the call checks exists
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, leader) } == 0 {
        end();
    }
}

/// A shell command line, as a ninja rule's command, is not run on Windows, where an edge runs a program, as each which
/// jig writes does.
///
/// # Errors
///
/// Always.
pub fn shell(line: &str) -> std::io::Result<std::process::Command> {
    let _ = line;
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a shell command line is not run on Windows, where an edge runs a program",
    ))
}

/// Whether a file is executable by a mode, which no file on Windows is.
pub fn is_executable(metadata: &std::fs::Metadata) -> bool {
    let _ = metadata;
    false
}

/// Make a file executable, or not, which needs nothing on Windows.
///
/// # Errors
///
/// Never, though it can on Unix.
#[allow(clippy::unnecessary_wraps, reason = "as on Unix, where it can fail")]
pub fn set_executable(path: &Path, executable: bool) -> std::io::Result<()> {
    let _ = (path, executable);
    Ok(())
}

/// The tokens of a jobserver: the count of a named semaphore.
pub struct Tokens {
    semaphore: usize,
}
// SAFETY: a semaphore's handle may be used from any thread
unsafe impl Send for Tokens {}
// SAFETY: as above, and its calls are atomic
unsafe impl Sync for Tokens {}

impl Tokens {
    /// Make a jobserver's semaphore with a name, holding a number of tokens, returning it and what gives it to a
    /// command, its name.
    ///
    /// # Errors
    ///
    /// If the semaphore cannot be created.
    pub fn create(name: &str, tokens: usize) -> std::io::Result<(Self, String)> {
        let count = i32::try_from(tokens).unwrap_or(i32::MAX);
        let wide = wide(name);
        // SAFETY: the name is a valid wide string, which outlives the call
        let semaphore = unsafe { CreateSemaphoreW(std::ptr::null_mut(), count, count.max(1), wide.as_ptr()) };
        if semaphore.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok((Self { semaphore: semaphore as usize }, name.to_string()))
    }
    /// Join the tokens of a jobserver by what gives it to a command, if it is the name of a semaphore, rather than a
    /// FIFO or file descriptors.
    #[must_use]
    pub fn join(auth: &str) -> Option<Self> {
        if auth.starts_with("fifo:") || auth.contains(',') || auth.is_empty() {
            return None;
        }
        let wide = wide(auth);
        // SAFETY: the name is a valid wide string, which outlives the call
        let semaphore = unsafe { OpenSemaphoreW(SYNCHRONIZE | SEMAPHORE_MODIFY_STATE, 0, wide.as_ptr()) };
        (!semaphore.is_null()).then_some(Self { semaphore: semaphore as usize })
    }
    /// Take a token, if one is free, without waiting for one.
    #[must_use]
    pub fn try_acquire(&self) -> Option<u8> {
        // SAFETY: the handle is open while the tokens are
        (unsafe { WaitForSingleObject(self.semaphore as Handle, 0) } == WAIT_OBJECT_0).then_some(b'+')
    }
    /// Give back a token.
    pub fn release(&self, token: u8) {
        let _ = token;
        // SAFETY: the handle is open while the tokens are, and the previous count is not asked for
        unsafe { ReleaseSemaphore(self.semaphore as Handle, 1, std::ptr::null_mut()) };
    }
}
impl Drop for Tokens {
    fn drop(&mut self) {
        // SAFETY: the handle is closed once, as the tokens are dropped
        unsafe { CloseHandle(self.semaphore as Handle) };
    }
}

/// Make a file share the contents of another, which Windows has no reflinks for.
///
/// # Errors
///
/// Always, as unsupported.
pub fn clone_file(from: &std::fs::File, to: &std::fs::File) -> std::io::Result<()> {
    let _ = (from, to);
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn ends_a_group_by_its_job() {
        // `ping`, which takes half a minute, in a group of its own, which is put in a job once it starts
        for signal in [SIGKILL, SIGINT] {
            let mut command = std::process::Command::new("ping");
            command.args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null());
            new_group(&mut command);
            let mut child = command.spawn().unwrap();
            group_started(child.id());
            std::thread::sleep(Duration::from_millis(500));
            let signalled = Instant::now();
            // which a Ctrl-Break event, or where there is no console, its job, ends, as killing it does
            signal_group(child.id(), signal);
            let status = child.wait().unwrap();
            group_ended(child.id());
            assert!(!status.success(), "{signal}");
            assert!(signalled.elapsed() < Duration::from_secs(10), "{signal}: {:?}", signalled.elapsed());
        }
        // and what has ended is not in the list of jobs
        assert!(JOBS.lock().unwrap().is_empty());
    }

    #[test]
    fn is_not_a_console_where_the_output_is_not() {
        assert_eq!(terminal_width(3), None);
    }
}
