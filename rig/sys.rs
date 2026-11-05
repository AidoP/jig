//! What rig needs of the system which `std` does not provide: signals, process groups, FIFOs and reflinks.
//!
//! The `unsafe` code of rig is here, behind a safe interface.

use std::ffi::{CString, c_int};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

unsafe extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
    fn kill(pid: c_int, signal: c_int) -> c_int;
    fn mkfifo(path: *const std::ffi::c_char, mode: ModeT) -> c_int;
    fn ioctl(fd: c_int, request: std::ffi::c_ulong, ...) -> c_int;
}

/// The size of a terminal, as `TIOCGWINSZ` gives it.
#[repr(C)]
#[derive(Default)]
struct Winsize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

/// The request of `ioctl` which gives the size of a terminal.
#[cfg(target_os = "macos")]
const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
#[cfg(not(target_os = "macos"))]
const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;

/// Returns the width of the terminal a file descriptor is, in columns, or `None` if it is not one.
pub fn terminal_width(fd: c_int) -> Option<usize> {
    let mut size = Winsize::default();
    // SAFETY: the request writes a `winsize`, which `size` is, and has no other effect
    let result = unsafe { ioctl(fd, TIOCGWINSZ, &raw mut size) };
    (result == 0 && size.columns > 0).then_some(usize::from(size.columns))
}

/// The request of `ioctl` which makes a file share the extents of another, `_IOW(0x94, 9, int)`.
#[cfg(target_os = "linux")]
const FICLONE: std::ffi::c_ulong = 0x4004_9409;

#[cfg(target_os = "macos")]
type ModeT = u16;
#[cfg(not(target_os = "macos"))]
type ModeT = u32;

pub const SIGHUP: c_int = 1;
pub const SIGINT: c_int = 2;
pub const SIGKILL: c_int = 9;
pub const SIGTERM: c_int = 15;

/// The flag of `open` which makes reads and writes not block, for a FIFO.
#[cfg(target_os = "macos")]
pub const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(target_os = "macos"))]
pub const O_NONBLOCK: i32 = 0o4000;

const SIG_ERR: usize = usize::MAX;

/// The signal which interrupted a build, or `0`.
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: c_int) {
    // storing to an atomic is safe in a signal handler
    INTERRUPTED.store(signal, Ordering::SeqCst);
}

/// Catches the signals which interrupt a build while it is held, rather than letting them end the process, so that
/// the build can stop the commands it is running and wait for them. The previous handlers are restored when it is
/// dropped.
pub struct Interrupts {
    previous: Vec<(c_int, usize)>,
}
impl Interrupts {
    #[must_use]
    pub fn catch() -> Self {
        INTERRUPTED.store(0, Ordering::SeqCst);
        let mut previous = Vec::new();
        for signum in [SIGINT, SIGTERM, SIGHUP] {
            // SAFETY: the handler only stores to an atomic, which is async-signal-safe
            let handler = unsafe { signal(signum, on_signal as extern "C" fn(c_int) as usize) };
            if handler != SIG_ERR {
                previous.push((signum, handler));
            }
        }
        Self { previous }
    }
    /// Returns the signal which interrupted the build since it started catching them, if one has, which none can if
    /// no handler could be installed.
    #[must_use]
    pub fn interrupted(&self) -> Option<c_int> {
        if self.previous.is_empty() {
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
        for &(signum, handler) in &self.previous {
            // SAFETY: the handler is one `signal` returned, so it was valid for the signal
            unsafe { signal(signum, handler) };
        }
    }
}

/// Send a signal to the process group of a process, which leads it.
pub fn signal_group(leader: u32, signum: c_int) {
    let Ok(pid) = c_int::try_from(leader) else {
        return;
    };
    // SAFETY: `kill` has no memory safety requirements, and a negative pid is the group of its absolute value, which
    // is never 0 or -1 here, as a child's pid is positive
    if pid > 1 {
        unsafe { kill(-pid, signum) };
    }
}

/// Create a FIFO at a path, readable and writable by its owner.
///
/// # Errors
///
/// If the path has a NUL byte, or the FIFO cannot be created.
pub fn make_fifo(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: the path is a valid C string, which outlives the call
    if unsafe { mkfifo(path.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Make a file share the contents of another, as a reflink, which the file system copies only once one of them is
/// written, as Btrfs, XFS and others can.
///
/// # Errors
///
/// If the file system cannot, the files are on different file systems, or the platform has no reflinks.
pub fn clone_file(from: &std::fs::File, to: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: both descriptors are open for as long as the call, as the files are borrowed, and `FICLONE` takes the
        // descriptor of the source as an `int`
        if unsafe { ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (from, to);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
