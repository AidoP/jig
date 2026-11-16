//! The wrapper `jig install --proxy` installs for a program on Windows, which cannot run a shell script. Like the
//! script used on Unix, it builds the program if needed and then runs it, or runs itself again if the build
//! reinstalled it.
//!
//! jig compiles it once for a project, in the build directory. Each installed wrapper is a copy of it with what it
//! wraps appended after the end of the executable, which Windows ignores when it runs it: the project root, the
//! `jig` to build with, the prefix, the wrapper's own path, and the program, each on a line, then the length of
//! those in bytes, and [`MAGIC`].

use std::process::{Command, Stdio};

/// What ends a wrapper.
const MAGIC: &[u8; 8] = b"JIGPROXY";

/// The exit status of `jig build --proxy` when it has reinstalled the wrapper which ran it, which then runs the new
/// wrapper instead.
const REINSTALLED: i32 = 75;

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("jig proxy: {message}");
            std::process::exit(1);
        },
    }
}

/// Build the program, then run it, returning its exit code.
fn run() -> Result<i32, String> {
    let [root, jig, prefix, wrapper, program] = wrapped()?;
    ignore_interrupts();
    // the build's output is written to stderr, so that the program's output is its own
    let built = Command::new(&jig)
        .args(["build", "--proxy", &prefix, &wrapper])
        .current_dir(&root)
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|error| format!("failed to run {jig:?}: {error}"))?;
    let next = match built.code() {
        Some(0) => program,
        Some(REINSTALLED) => wrapper,
        _ => return Ok(1),
    };
    let ran = Command::new(&next).args(std::env::args_os().skip(1)).status();
    let ran = ran.map_err(|error| format!("failed to run {next:?}: {error}"))?;
    Ok(ran.code().unwrap_or(1))
}

/// Returns what the wrapper wraps, from after its end.
fn wrapped() -> Result<[String; 5], String> {
    let exe = std::env::current_exe().map_err(|error| format!("failed to find itself: {error}"))?;
    let bytes = std::fs::read(&exe).map_err(|error| format!("failed to read {exe:?}: {error}"))?;
    let invalid = || format!("{exe:?} is not a wrapper installed by `jig install --proxy`");
    let rest = bytes.strip_suffix(MAGIC).ok_or_else(invalid)?;
    let (rest, length) = rest.split_at_checked(rest.len().checked_sub(4).ok_or_else(invalid)?).ok_or_else(invalid)?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| invalid())?) as usize;
    let text = rest.get(rest.len().checked_sub(length).ok_or_else(invalid)?..).ok_or_else(invalid)?;
    let text = std::str::from_utf8(text).map_err(|_| invalid())?;
    let lines: Vec<String> = text.split('\n').map(String::from).collect();
    lines.try_into().map_err(|_| invalid())
}

/// Leave an interrupt, as Ctrl-C, to the program, which the console gives it as well, so that the wrapper waits for it
/// to end, and exits as it does, rather than ending first.
#[cfg(windows)]
fn ignore_interrupts() {
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }
    // handled, which the handler, unlike ignoring every event, is not inherited by what the wrapper runs
    extern "system" fn handled(_: u32) -> i32 {
        1
    }
    // SAFETY: the handler is a function which lives as long as the process
    unsafe {
        SetConsoleCtrlHandler(Some(handled), 1);
    }
}
#[cfg(not(windows))]
fn ignore_interrupts() {}
