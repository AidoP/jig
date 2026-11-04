//! A GNU make jobserver, which shares the jobs of a build with the commands it runs, such as rustc's codegen threads,
//! and `make` and `cargo` run as commands, so that together they run no more at once than the build allows.
//!
//! A jobserver is a FIFO holding a byte, a token, for each job beyond the first. Each command holds one job without a
//! token, and takes a token from the FIFO for each other job it runs, which it writes back once the job is done. rig
//! takes a token for each edge it runs beyond the first, and gives the commands the FIFO in `MAKEFLAGS` and
//! `CARGO_MAKEFLAGS`, as `--jobserver-auth=fifo:<path>`, as GNU make 4.4 does.
//!
//! When rig is itself run by a jobserver which is a FIFO, it takes its tokens from that one instead, and leaves the
//! environment of its commands as it is. A jobserver of file descriptors, which older versions of make give, is not
//! joined, and rig creates its own.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Command;

use crate::sys;

pub struct Jobserver {
    fifo: std::fs::File,
    /// The FIFO rig created, which is removed when the jobserver is dropped, or `None` if it joined another.
    created: Option<PathBuf>,
    /// The value of `MAKEFLAGS` for the commands, if rig created the jobserver.
    makeflags: Option<String>,
}

impl Jobserver {
    /// Create a jobserver for a number of jobs, in the temporary directory.
    ///
    /// # Errors
    ///
    /// If the FIFO cannot be created, opened or filled.
    pub fn create(jobs: usize) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("rig-jobserver-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed));
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        sys::make_fifo(&path)?;
        let mut server = match open(&path) {
            Ok(fifo) => Self { fifo, created: Some(path.clone()), makeflags: None },
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            },
        };
        server.fifo.write_all(&vec![b'+'; jobs.saturating_sub(1)])?;
        server.makeflags = Some(format!("-j{jobs} --jobserver-auth=fifo:{}", path.display()));
        Ok(server)
    }
    /// Join the jobserver which rig is run by, from `MAKEFLAGS` or `CARGO_MAKEFLAGS`, if it is a FIFO.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        ["CARGO_MAKEFLAGS", "MAKEFLAGS"].into_iter().find_map(|variable| {
            let flags = std::env::var(variable).ok()?;
            let path = fifo_path(&flags)?;
            Some(Self { fifo: open(path.as_ref()).ok()?, created: None, makeflags: None })
        })
    }
    /// Take a token for a job, if one is free, without waiting for one.
    #[must_use]
    pub fn try_acquire(&self) -> Option<u8> {
        let mut token = [0];
        match (&self.fifo).read(&mut token) {
            Ok(1) => Some(token[0]),
            _ => None,
        }
    }
    /// Give back a token, once its job is done.
    pub fn release(&self, token: u8) {
        let _ = (&self.fifo).write_all(&[token]);
    }
    /// The value of `MAKEFLAGS` which gives a command the jobserver, if rig created it, as one it joined is already in
    /// the environment.
    #[must_use]
    pub fn makeflags(&self) -> Option<&str> {
        self.makeflags.as_deref()
    }
    /// Give a command the jobserver, if rig created it, as one it joined is already in the environment.
    pub fn apply(&self, command: &mut Command) {
        if let Some(makeflags) = &self.makeflags {
            command.env("MAKEFLAGS", makeflags).env("CARGO_MAKEFLAGS", makeflags);
        }
    }
}
impl Drop for Jobserver {
    fn drop(&mut self) {
        if let Some(path) = &self.created {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Open a FIFO for reading and writing, which does not wait for another end, and whose reads do not block.
fn open(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::options().read(true).write(true).custom_flags(sys::O_NONBLOCK).open(path)
}

/// Returns the path of the FIFO a jobserver is given by, in flags as `MAKEFLAGS` has them, of which the last is used.
fn fifo_path(flags: &str) -> Option<String> {
    let auth = flags.split_whitespace().filter_map(|flag| flag.strip_prefix("--jobserver-auth=")).next_back()?;
    auth.strip_prefix("fifo:").map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_a_token_for_each_job_beyond_the_first() {
        let server = Jobserver::create(3).unwrap();
        let (a, b) = (server.try_acquire(), server.try_acquire());
        assert_eq!((a, b), (Some(b'+'), Some(b'+')));
        assert_eq!(server.try_acquire(), None);
        server.release(b'+');
        assert_eq!(server.try_acquire(), Some(b'+'));
        let path = server.created.clone().unwrap();
        assert!(server.makeflags.as_ref().unwrap().starts_with("-j3 --jobserver-auth=fifo:"));
        drop(server);
        assert!(!path.exists());
    }

    #[test]
    fn finds_a_fifo_in_the_flags() {
        assert_eq!(fifo_path("-j8 --jobserver-auth=fifo:/tmp/x"), Some("/tmp/x".into()));
        assert_eq!(fifo_path("-j8 --jobserver-auth=3,4"), None);
        assert_eq!(fifo_path("--jobserver-auth=3,4 --jobserver-auth=fifo:/a"), Some("/a".into()));
        assert_eq!(fifo_path("-k"), None);
    }

    #[test]
    fn commands_share_the_tokens() {
        let server = Jobserver::create(2).unwrap();
        let mut command = Command::new("/bin/sh");
        // a command takes the token, as a job would, and gives it back
        let script = "f=${MAKEFLAGS#*fifo:}; head -c 1 \"$f\" > /dev/null && printf + > \"$f\"";
        command.args(["-c", script]);
        server.apply(&mut command);
        assert!(command.status().unwrap().success());
        assert_eq!(server.try_acquire(), Some(b'+'));
    }
}
