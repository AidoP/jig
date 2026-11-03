//! What builds a configured project's graph, which the commands use rather than running a build tool themselves, so
//! that they do not depend on which one it is.

use std::ffi::OsString;

use crate::Error;

/// Where the output of a build is written.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Stdout,
    /// Stderr, so that stdout is left for what follows, such as a program which is run, or JSON diagnostics.
    Stderr,
}

/// What is built, and how.
#[derive(Default)]
pub struct Request<'a> {
    /// The targets to build, or the defaults if there are none.
    pub targets: &'a [&'a str],
    /// Arguments for the build tool, as `jig build` is given them, before the targets.
    pub args: &'a [OsString],
    /// Show only what the commands print, rather than the progress of the build.
    pub quiet: bool,
    /// Build what can be built after a command fails, rather than stopping.
    pub keep_going: bool,
    pub output: Output,
}

pub trait Builder {
    /// Build what is requested, returning whether it was built, or failing if the build could not be run.
    fn build(&self, request: &Request) -> Result<bool, Error>;
    /// Generate the graph again if it is out of date, which writes what it shows to `output`, except that nothing was
    /// out of date.
    fn regenerate(&self, quiet: bool, output: Output) -> Result<(), Error>;
    /// Returns whether a target is up to date, without building it or generating the graph again.
    fn up_to_date(&self, target: &str) -> Result<bool, Error>;
    /// Returns whether the graph is up to date, without generating it again.
    fn graph_up_to_date(&self) -> Result<bool, Error>;
}
