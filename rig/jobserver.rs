//! A GNU make jobserver, which shares the jobs of a build with the commands it runs, such as rustc's codegen threads,
//! and `make` and `cargo` run as commands, so that together they run no more at once than the build allows.
//!
//! A jobserver holds a token for each job beyond the first: on Unix a byte in a FIFO, and on Windows a count of a
//! named semaphore, as GNU make has them. Each command holds one job without a token, and takes a token for each other
//! job it runs, which it gives back once the job is done. rig takes a token for each edge it runs beyond the first, and
//! gives the commands the jobserver in `MAKEFLAGS` and `CARGO_MAKEFLAGS`, as `--jobserver-auth=fifo:<path>` on Unix,
//! as GNU make 4.4 does, or `--jobserver-auth=<name>` on Windows.
//!
//! When rig is itself run by a jobserver which it can join, it takes its tokens from that one instead, and leaves the
//! environment of its commands as it is. A jobserver of file descriptors, which older versions of make give on Unix,
//! is not joined, and rig creates its own.

use std::process::Command;

use crate::sys;

pub struct Jobserver {
    tokens: sys::Tokens,
    /// The value of `MAKEFLAGS` for the commands, if rig created the jobserver.
    makeflags: Option<String>,
}

impl Jobserver {
    /// Create a jobserver for a number of jobs: a FIFO in the temporary directory, or a semaphore.
    ///
    /// # Errors
    ///
    /// If it cannot be created, opened or filled.
    pub fn create(jobs: usize) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("rig-jobserver-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed));
        let (tokens, auth) = sys::Tokens::create(&name, jobs.saturating_sub(1))?;
        Ok(Self { tokens, makeflags: Some(format!("-j{jobs} --jobserver-auth={auth}")) })
    }
    /// Join the jobserver which rig is run by, from `MAKEFLAGS` or `CARGO_MAKEFLAGS`, if it can be joined.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        ["CARGO_MAKEFLAGS", "MAKEFLAGS"].into_iter().find_map(|variable| {
            let flags = std::env::var(variable).ok()?;
            Some(Self { tokens: sys::Tokens::join(auth(&flags)?)?, makeflags: None })
        })
    }
    /// Take a token for a job, if one is free, without waiting for one.
    #[must_use]
    pub fn try_acquire(&self) -> Option<u8> {
        self.tokens.try_acquire()
    }
    /// Give back a token, once its job is done.
    pub fn release(&self, token: u8) {
        self.tokens.release(token);
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

/// Returns what a jobserver is given by, in flags as `MAKEFLAGS` has them, of which the last is used.
fn auth(flags: &str) -> Option<&str> {
    flags.split_whitespace().filter_map(|flag| flag.strip_prefix("--jobserver-auth=")).next_back()
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
        let makeflags = server.makeflags.clone().unwrap();
        // another joins it by what it is given
        let joined = sys::Tokens::join(auth(&makeflags).unwrap()).unwrap();
        assert_eq!(joined.try_acquire(), None);
        server.release(b'+');
        assert_eq!(joined.try_acquire(), Some(b'+'));
        drop(joined);
        #[cfg(unix)]
        {
            let path = makeflags.strip_prefix("-j3 --jobserver-auth=fifo:").unwrap().to_string();
            drop(server);
            assert!(!std::path::Path::new(&path).exists());
        }
    }

    #[test]
    fn finds_the_jobserver_in_the_flags() {
        assert_eq!(auth("-j8 --jobserver-auth=fifo:/tmp/x"), Some("fifo:/tmp/x"));
        assert_eq!(auth("--jobserver-auth=3,4 --jobserver-auth=fifo:/a"), Some("fifo:/a"));
        assert_eq!(auth("-k"), None);
        // a jobserver of file descriptors is not joined
        assert!(sys::Tokens::join("3,4").is_none());
    }

    #[cfg(unix)]
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
