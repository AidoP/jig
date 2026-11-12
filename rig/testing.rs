//! Graphs for tests, built through rig's API, whose edges run the [test helper](crate::test_helper).
//!
//! A rule is a command and what else its edges have, each a template, in which `$in` and `$out` are the edge's inputs
//! and outputs, as words of their own, and `$<name>` a variable of the edge or the graph, with `$$` for a `$`. Its
//! `command` is the helper's steps, and its `discover` those which write the depfile without running it, while
//! `description`, which is the command as it is written without one, and `depfile` are text. `pool = console`,
//! `generator` and `restat` set what they name.

use std::collections::HashMap;

use crate::graph::{Edge, EdgeKind, Exec, Graph, Pool};
use crate::test_helper;

/// Returns a graph to build, with its rules and edges added in turn.
pub(crate) fn graph() -> Graphs {
    Graphs::default()
}

/// A graph being built: its rules and variables, whether the edges added are hashed, what identifies them in a cache
/// if they are cached, which workers they may run on, and the outputs the next edge reports early.
#[derive(Default)]
pub(crate) struct Graphs {
    graph: Graph,
    rules: HashMap<String, Vec<(String, String)>>,
    variables: HashMap<String, String>,
    hashed: bool,
    cache: Option<String>,
    placement: crate::Placement,
    early: Vec<String>,
}

impl Graphs {
    pub(crate) fn variable(mut self, name: &str, value: &str) -> Self {
        self.variables.insert(name.into(), value.into());
        self
    }
    /// Stamp the files of the edges added from now on by their contents.
    pub(crate) fn hashed(mut self) -> Self {
        self.hashed = true;
        self
    }
    /// Cache the results of the edges added from now on, identified by `key` beside their actions and inputs.
    pub(crate) fn cached(mut self, key: &str) -> Self {
        self.cache = Some(key.into());
        self
    }
    /// Place the edges added from now on, which workers they may run on.
    pub(crate) fn placed(mut self, placement: crate::Placement) -> Self {
        self.placement = placement;
        self
    }
    /// Have the next edge report these of its outputs before it finishes, as rustc does.
    pub(crate) fn early(mut self, outputs: &[&str]) -> Self {
        self.early = outputs.iter().map(|&output| output.into()).collect();
        self
    }
    pub(crate) fn rule(mut self, name: &str, bindings: &[(&str, &str)]) -> Self {
        let bindings = bindings.iter().map(|&(key, value)| (key.into(), value.into())).collect();
        self.rules.insert(name.into(), bindings);
        self
    }
    /// Add an edge by a rule, or an alias, by `phony`, whose outputs are followed by `|` and its implicit outputs, and
    /// whose inputs are followed by `|` and its implicit inputs, then `||` and its order-only inputs.
    pub(crate) fn edge(self, rule: &str, outputs: &[&str], inputs: &[&str]) -> Self {
        self.edge_with(rule, outputs, inputs, &[])
    }
    /// Add an edge with variables, as [`Self::edge`] does.
    pub(crate) fn edge_with(
        mut self,
        rule: &str,
        outputs: &[&str],
        inputs: &[&str],
        variables: &[(&str, &str)],
    ) -> Self {
        let [outputs, implicit_outputs, _] = split(outputs);
        let [inputs, implicit, order_only] = split(inputs);
        let mut nodes = |paths: &[&str]| paths.iter().map(|path| self.graph.file(path)).collect::<Vec<_>>();
        let mut edge = Edge {
            outputs: nodes(&outputs),
            implicit_outputs: nodes(&implicit_outputs),
            inputs: nodes(&inputs),
            implicit: nodes(&implicit),
            order_only: nodes(&order_only),
            ..Edge::default()
        };
        if rule != "phony" {
            let scope = Scope { outputs: &outputs, inputs: &inputs, edge: variables, graph: &self.variables };
            let bindings = &self.rules[rule];
            let binding = |key: &str| bindings.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
            let command = binding("command").unwrap_or_else(|| panic!("the rule {rule:?} has no command"));
            edge.kind = EdgeKind::Command(scope.program(command));
            edge.description = binding("description").map_or_else(|| scope.text(command), |text| scope.text(text));
            edge.depfile = binding("depfile").map(|depfile| scope.text(depfile));
            edge.discover = binding("discover").map(|discover| scope.program(discover));
            edge.pool = if binding("pool") == Some("console") { Pool::Console } else { Pool::Default };
            edge.generator = binding("generator").is_some();
            edge.restat = binding("restat").is_some();
            edge.hashed = self.hashed;
            edge.cache.clone_from(&self.cache);
            edge.placement = self.placement.clone();
            let early = std::mem::take(&mut self.early);
            let node = |path: &String| self.graph.node(path).expect("an early output is an output");
            edge.early = early.iter().map(node).collect();
        }
        self.graph.add_edge(edge).unwrap();
        self
    }
    pub(crate) fn finish(self) -> Graph {
        self.graph
    }
}

/// What the templates of an edge are expanded in.
struct Scope<'a> {
    outputs: &'a [&'a str],
    inputs: &'a [&'a str],
    edge: &'a [(&'a str, &'a str)],
    graph: &'a HashMap<String, String>,
}
impl Scope<'_> {
    /// Returns the helper's program, which runs the steps of a template.
    fn program(&self, template: &str) -> Exec {
        let mut words = Vec::new();
        for word in test_helper::words(template) {
            match word.as_str() {
                "$in" => words.extend(self.inputs.iter().map(|&input| input.to_string())),
                "$out" => words.extend(self.outputs.iter().map(|&output| output.to_string())),
                _ => words.push(self.expand(&word)),
            }
        }
        test_helper::exec(words)
    }
    /// Returns a template as text, with paths joined by spaces.
    fn text(&self, template: &str) -> String {
        self.expand(template)
    }
    fn expand(&self, template: &str) -> String {
        let mut text = String::new();
        let mut rest = template;
        while let Some(at) = rest.find('$') {
            text.push_str(&rest[..at]);
            rest = &rest[at + 1..];
            if let Some(after) = rest.strip_prefix('$') {
                text.push('$');
                rest = after;
                continue;
            }
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-').unwrap_or(rest.len());
            let name = &rest[..end];
            rest = &rest[end..];
            let value = match name {
                "in" => self.inputs.join(" "),
                "out" => self.outputs.join(" "),
                name => self
                    .edge
                    .iter()
                    .find(|(variable, _)| *variable == name)
                    .map(|(_, value)| (*value).to_string())
                    .or_else(|| self.graph.get(name).cloned())
                    .unwrap_or_default(),
            };
            text.push_str(&value);
        }
        text + rest
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
