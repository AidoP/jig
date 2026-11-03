//! The build graph which `jig generate` describes, apart from the file it is written to, which [`crate::ninja`] writes.
//!
//! The commands of rules are still templates, which refer to `$in`, `$out` and the variables of their edges, and run
//! with `sh`.

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
