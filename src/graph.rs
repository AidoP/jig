//! The build graph which `jig generate` describes, apart from what it is written to: `build.ninja`, which
//! [`crate::ninja`] writes, and rig's graph, which [`crate::rig_graph`] saves.
//!
//! The commands of rules are still templates, which refer to `$in`, `$out` and the variables of their edges, as ninja
//! evaluates them, and run with `sh`.

use std::path::Path;

use crate::Error;

/// A rule, which edges are built by.
#[derive(Default)]
pub struct Rule<'a> {
    pub name: &'a str,
    pub command: &'a str,
    /// What is shown while an edge of the rule is built.
    pub description: &'a str,
    /// Whether an edge runs alone, with the terminal, rather than with its output captured.
    pub console: bool,
    /// The depfile which an edge writes, in the form gcc writes, of the inputs it discovered.
    pub depfile: Option<&'a str>,
    /// Whether an edge generates the graph, so that it is not rebuilt when its command changes.
    pub generator: bool,
}

impl Rule<'_> {
    /// Returns the bindings of the rule as ninja's, in the order they are written.
    pub fn bindings(&self) -> Vec<(&str, &str)> {
        let mut bindings = vec![("command", self.command), ("description", self.description)];
        if self.console {
            bindings.push(("pool", "console"));
        }
        if let Some(depfile) = self.depfile {
            bindings.extend([("deps", "gcc"), ("depfile", depfile)]);
        }
        if self.generator {
            bindings.push(("generator", "1"));
        }
        bindings
    }
}

/// An edge, which builds its outputs from its inputs by its rule.
#[derive(Default)]
pub struct Edge<'a> {
    pub rule: &'a str,
    pub outputs: &'a [&'a Path],
    pub implicit_outputs: &'a [&'a Path],
    pub inputs: &'a [&'a Path],
    /// Inputs which trigger a rebuild but are not part of `$in`.
    pub implicit: &'a [&'a Path],
    /// Inputs which must be built first but do not trigger a rebuild.
    pub order_only: &'a [&'a Path],
    pub variables: &'a [(&'a str, &'a str)],
    /// The program the edge runs, as rig runs it, without a shell, in place of the command of its rule, which
    /// `build.ninja` has.
    pub exec: Option<&'a rig::Exec>,
    /// Whether rig stamps the edge's files by their contents, which ninja cannot.
    pub hashed: bool,
    /// What identifies the edge's toolchain, if rig keeps its results in the cache, which ninja cannot.
    pub cache: Option<&'a str>,
    /// The environment variables the edge's command reads from those it inherits, which rig tracks, and passes on when
    /// it runs it in a hermetic environment, while ninja passes on every one.
    pub pass_env: &'a [String],
    /// Which workers rig may send the edge to, if it is cached.
    pub placement: rig::Placement,
    /// The outputs the edge reports before it finishes, as rustc does, which rig starts what needs only them once they
    /// are written by, and ninja does not know.
    pub early: &'a [&'a Path],
    /// What writes the edge's depfile without running it, by which rig finds what an edge which has not run reads, to
    /// send it to a worker, which ninja does not need.
    pub discover: Option<rig::Exec>,
}

/// What a build graph is written to.
///
/// Paths are relative to the build directory, where the graph is built.
pub trait Graph {
    /// Add a variable which the commands of the rules refer to.
    fn variable(&mut self, name: &str, value: &str) -> Result<(), Error>;
    fn rule(&mut self, rule: &Rule) -> Result<(), Error>;
    fn edge(&mut self, edge: &Edge) -> Result<(), Error>;
    /// Add a name which builds the targets, which is not a file.
    fn alias(&mut self, name: &Path, targets: &[&Path]) -> Result<(), Error>;
    /// Add to the targets which are built when none are requested.
    fn default(&mut self, targets: &[&Path]) -> Result<(), Error>;
    /// Finish writing the graph, which is only then used.
    fn finish(&mut self) -> Result<(), Error>;
}

/// Returns a path as the graph has it, with `/` between its components, as on Unix, whichever the platform joined them
/// with, so that a path is named the same however it was made, as `test/<name>` is.
pub fn path_string(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) { path.replace('\\', "/") } else { path.into_owned() }
}
