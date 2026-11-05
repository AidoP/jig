//! Graphs for tests, built from rules and edges through a [`Manifest`], as jig builds them.

use crate::Error;
use crate::graph::Graph;
use crate::ninja::{Build, Manifest};

/// Returns a graph to build, with its statements added in turn.
pub(crate) fn graph() -> Graphs {
    Graphs(Manifest::new(), false, None, crate::Placement::default(), Vec::new())
}

/// The statements of a graph, whether the edges added are hashed, what identifies them in a cache if they are cached,
/// which workers they may run on, and the outputs the next edge reports early.
pub(crate) struct Graphs(Manifest, bool, Option<String>, crate::Placement, Vec<String>);

impl Graphs {
    pub(crate) fn variable(mut self, name: &str, value: &str) -> Self {
        self.0.variable(name, value).unwrap();
        self
    }
    /// Stamp the files of the edges added from now on by their contents.
    pub(crate) fn hashed(mut self) -> Self {
        self.1 = true;
        self
    }
    /// Cache the results of the edges added from now on, identified by `key` beside their actions and inputs.
    pub(crate) fn cached(mut self, key: &str) -> Self {
        self.2 = Some(key.into());
        self
    }
    /// Place the edges added from now on, which workers they may run on.
    pub(crate) fn placed(mut self, placement: crate::Placement) -> Self {
        self.3 = placement;
        self
    }
    /// Have the next edge report these of its outputs before it finishes, as rustc does.
    pub(crate) fn early(mut self, outputs: &[&str]) -> Self {
        self.4 = outputs.iter().map(|&output| output.into()).collect();
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
        let early = std::mem::take(&mut self.4);
        let early: Vec<&str> = early.iter().map(String::as_str).collect();
        let build = Build {
            rule,
            outputs: &outputs,
            implicit_outputs: &implicit_outputs,
            inputs: &inputs,
            implicit: &implicit,
            order_only: &order_only,
            bindings,
            action: None,
            hashed: self.1,
            cache: self.2.clone(),
            pass_env: Vec::new(),
            placement: self.3.clone(),
            discover: None,
            early: &early,
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
