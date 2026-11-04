//! Graphs for tests, built from rules and edges through a [`Manifest`], as jig builds them.

use crate::Error;
use crate::graph::Graph;
use crate::ninja::{Build, Manifest};

/// Returns a graph to build, with its statements added in turn.
pub(crate) fn graph() -> Graphs {
    Graphs(Manifest::new())
}

pub(crate) struct Graphs(Manifest);

impl Graphs {
    pub(crate) fn variable(mut self, name: &str, value: &str) -> Self {
        self.0.variable(name, value).unwrap();
        self
    }
    pub(crate) fn rule(mut self, name: &str, bindings: &[(&str, &str)]) -> Self {
        self.0.rule(name, bindings).unwrap();
        self
    }
    /// Add an edge, whose outputs are followed by `|` and its implicit outputs, and whose inputs are followed by `|`
    /// and its implicit inputs, then `||` and its order-only inputs.
    pub(crate) fn edge(self, rule: &str, outputs: &[&str], inputs: &[&str]) -> Self {
        self.edge_with(rule, outputs, inputs, &[])
    }
    /// Add an edge with bindings, as [`Self::edge`] does.
    pub(crate) fn edge_with(
        mut self,
        rule: &str,
        outputs: &[&str],
        inputs: &[&str],
        bindings: &[(&str, &str)],
    ) -> Self {
        self.try_edge(rule, outputs, inputs, bindings).unwrap();
        self
    }
    /// Add an edge, as [`Self::edge`] does, returning why it could not be.
    pub(crate) fn try_edge(
        &mut self,
        rule: &str,
        outputs: &[&str],
        inputs: &[&str],
        bindings: &[(&str, &str)],
    ) -> Result<(), Error> {
        let [outputs, implicit_outputs, ..] = split(outputs);
        let [inputs, implicit, order_only] = split(inputs);
        let build = Build {
            rule,
            outputs: &outputs,
            implicit_outputs: &implicit_outputs,
            inputs: &inputs,
            implicit: &implicit,
            order_only: &order_only,
            bindings,
            action: None,
        };
        self.0.build(&build)
    }
    pub(crate) fn default(mut self, paths: &[&str]) -> Self {
        self.0.default(paths);
        self
    }
    pub(crate) fn finish(self) -> Graph {
        self.0.finish()
    }
}

/// Split paths at `|` and `||`, into those before them, after `|`, and after `||`.
fn split<'a>(paths: &[&'a str]) -> [Vec<&'a str>; 3] {
    let mut lists: [Vec<&str>; 3] = Default::default();
    let mut list = 0;
    for &path in paths {
        match path {
            "|" => list = 1,
            "||" => list = 2,
            path => lists[list].push(path),
        }
    }
    lists
}
