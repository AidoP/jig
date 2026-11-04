//! rig's graph, `graph` in the build directory, which is built from the same description as `build.ninja`.
//!
//! The rules and edges are evaluated as ninja evaluates them, by [`rig::ninja::Manifest`], so that both describe the
//! same build.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::graph::{Edge, Graph, Rule};

/// The file of the graph, in the build directory.
pub const FILE: &str = "graph";

pub struct RigGraph {
    path: PathBuf,
    manifest: rig::ninja::Manifest,
}
impl RigGraph {
    /// Build a graph to save at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path, manifest: rig::ninja::Manifest::new() }
    }
}

fn paths(paths: &[&Path]) -> Vec<String> {
    paths.iter().map(|path| path.to_string_lossy().into_owned()).collect()
}

fn strs(strings: &[String]) -> Vec<&str> {
    strings.iter().map(String::as_str).collect()
}

impl Graph for RigGraph {
    fn variable(&mut self, name: &str, value: &str) -> Result<(), Error> {
        self.manifest.variable(name, value).map_err(Error::Rig)
    }
    fn rule(&mut self, rule: &Rule) -> Result<(), Error> {
        self.manifest.rule(rule.name, &rule.bindings()).map_err(Error::Rig)
    }
    fn edge(&mut self, edge: &Edge) -> Result<(), Error> {
        let lists = [edge.outputs, edge.implicit_outputs, edge.inputs, edge.implicit, edge.order_only].map(paths);
        let [outputs, implicit_outputs, inputs, implicit, order_only] = lists.each_ref().map(|list| strs(list));
        self.manifest
            .build(&rig::ninja::Build {
                rule: edge.rule,
                outputs: &outputs,
                implicit_outputs: &implicit_outputs,
                inputs: &inputs,
                implicit: &implicit,
                order_only: &order_only,
                bindings: edge.variables,
            })
            .map_err(Error::Rig)
    }
    fn alias(&mut self, name: &Path, targets: &[&Path]) -> Result<(), Error> {
        self.edge(&Edge { rule: "phony", outputs: &[name], inputs: targets, ..Default::default() })
    }
    fn default(&mut self, targets: &[&Path]) -> Result<(), Error> {
        self.manifest.default(&strs(&paths(targets)));
        Ok(())
    }
    /// Save the graph, and record the edge which generates it as up to date.
    fn finish(&mut self) -> Result<(), Error> {
        let graph = std::mem::take(&mut self.manifest).finish();
        graph.save(&self.path).map_err(Error::Rig)?;
        match self.path.parent() {
            Some(build_dir) => crate::rig_builder::record_generator(&graph, build_dir),
            None => Ok(()),
        }
    }
}
