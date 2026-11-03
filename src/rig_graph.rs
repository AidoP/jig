//! rig's graph, `graph` in the build directory, which is built from the same description as `build.ninja`.
//!
//! The rules and edges are evaluated as ninja evaluates them, by [`rig::ninja::Manifest`], so that both describe the
//! same build. With [`VERIFY`] set, the `build.ninja` written beside the graph is read back by rig and compared with
//! it, which the integration tests do for every project they configure.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::graph::{Edge, Graph, Rule};

/// The file of the graph, in the build directory.
pub const FILE: &str = "graph";

/// The environment variable which, if it is set and not empty, has the graph compared with `build.ninja`.
pub const VERIFY: &str = "JIG_VERIFY_GRAPH";

pub struct RigGraph {
    path: PathBuf,
    manifest: rig::ninja::Manifest,
    /// The `build.ninja` to compare the graph with, once both are written.
    verify: Option<PathBuf>,
}
impl RigGraph {
    /// Build a graph to save at `path`, compared with the ninja file `ninja` if [`VERIFY`] is set.
    pub fn new(path: PathBuf, ninja: &Path) -> Self {
        let verify = std::env::var_os(VERIFY).is_some_and(|value| !value.is_empty()).then(|| ninja.to_path_buf());
        Self { path, manifest: rig::ninja::Manifest::new(), verify }
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
    /// Save the graph, then compare it with `build.ninja` if [`VERIFY`] is set.
    fn finish(&mut self) -> Result<(), Error> {
        let graph = std::mem::take(&mut self.manifest).finish();
        graph.save(&self.path).map_err(Error::Rig)?;
        let Some(ninja) = &self.verify else {
            return Ok(());
        };
        let text = match std::fs::read(ninja) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => return Err(Error::Read(ninja.clone(), error)),
        };
        let read = rig::ninja::read(&text).map_err(Error::Rig)?;
        match graph.diff(&read) {
            Some(difference) => Err(Error::GraphMismatch(ninja.clone(), difference)),
            None => Ok(()),
        }
    }
}
