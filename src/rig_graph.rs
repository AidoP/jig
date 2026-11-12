//! rig's graph, `graph` in the build directory, which `jig generate` builds through rig's own API: each edge with the
//! program it runs, its description and its depfile given directly.
//!
//! Paths are relative to the build directory, where the graph is built.

use std::path::{Path, PathBuf};

use crate::Error;

/// The file of the graph, in the build directory.
pub const FILE: &str = "graph";

/// An edge, which runs a program to build its outputs from its inputs.
#[derive(Default)]
pub struct Edge<'a> {
    /// What is shown while the edge runs.
    pub description: String,
    /// The files the edge writes, the first of which names it.
    pub outputs: &'a [&'a Path],
    /// The files the edge reads, which must be there, or be built: a crate's sources, a custom command's inputs, its
    /// program, given by a path, a linker script and a target's specification.
    pub inputs: &'a [&'a Path],
    /// The files the edge depends on, which may be missing, as the crates a crate uses, one of which may have been
    /// removed along with its use.
    pub dependencies: &'a [&'a Path],
    /// The files which are built before the edge, but which it neither reads nor depends on, as a crate's or a
    /// command's `depends`.
    pub after: &'a [&'a Path],
    /// The program the edge runs.
    pub exec: rig::Exec,
    /// The depfile the program writes, in the form gcc writes, of the inputs it discovered.
    pub depfile: Option<&'a Path>,
    /// Whether the edge runs alone, with the terminal, rather than with its output captured.
    pub console: bool,
    /// Whether the edge generates the graph, so that it is brought up to date before anything else.
    pub generator: bool,
    /// Whether rig stamps the edge's files by their contents.
    pub hashed: bool,
    /// What identifies the edge's toolchain, if rig keeps its results in the cache.
    pub cache: Option<&'a str>,
    /// The environment variables the edge's program reads from those it inherits, which rig tracks, and passes on when
    /// it runs it in a hermetic environment.
    pub pass_env: &'a [String],
    /// Which workers rig may send the edge to, if it is cached.
    pub placement: rig::Placement,
    /// The outputs the edge reports before it finishes, as rustc does, which rig starts what needs only them once they
    /// are written by. Each is one of its outputs.
    pub early: &'a [&'a Path],
    /// What writes the edge's depfile without running it, by which rig finds what an edge which has not run reads, to
    /// send it to a worker.
    pub discover: Option<rig::Exec>,
}

/// Builds rig's graph, to save at a path.
pub struct RigGraph {
    path: PathBuf,
    graph: rig::Graph,
}
impl RigGraph {
    /// Build a graph to save at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path, graph: rig::Graph::new() }
    }
    /// Returns the nodes of paths, adding those which are not in the graph.
    fn nodes(&mut self, paths: &[&Path]) -> Vec<rig::NodeId> {
        paths.iter().map(|path| self.graph.file(&path_string(path))).collect()
    }
    pub fn edge(&mut self, edge: Edge) -> Result<(), Error> {
        let outputs = self.nodes(edge.outputs);
        let early = self.nodes(edge.early);
        debug_assert!(early.iter().all(|node| outputs.contains(node)));
        let edge = rig::Edge {
            kind: rig::EdgeKind::Command(edge.exec),
            description: edge.description,
            outputs,
            inputs: self.nodes(edge.inputs),
            dependencies: self.nodes(edge.dependencies),
            after: self.nodes(edge.after),
            depfile: edge.depfile.map(path_string),
            pool: if edge.console { rig::Pool::Console } else { rig::Pool::Default },
            generator: edge.generator,
            restat: false,
            hashed: edge.hashed,
            cache: edge.cache.map(String::from),
            pass_env: edge.pass_env.to_vec(),
            placement: edge.placement,
            discover: edge.discover,
            early,
        };
        self.graph.add_edge(edge).map(|_| ()).map_err(Error::Rig)
    }
    /// Add a name which builds the targets, which is not a file.
    pub fn alias(&mut self, name: &Path, targets: &[&Path]) -> Result<(), Error> {
        let targets = self.nodes(targets);
        self.graph.add_alias(&path_string(name), &targets).map(|_| ()).map_err(Error::Rig)
    }
    /// Add to the targets which are built when none are requested.
    pub fn default(&mut self, targets: &[&Path]) {
        let targets = self.nodes(targets);
        self.graph.add_default(&targets);
    }
    /// Save the graph, and record the edge which generates it as up to date.
    pub fn finish(self) -> Result<(), Error> {
        self.graph.save(&self.path).map_err(Error::Rig)?;
        match self.path.parent() {
            Some(build_dir) => crate::rig_builder::record_generator(&self.graph, build_dir),
            None => Ok(()),
        }
    }
}

/// Returns a path as the graph has it, with `/` between its components, as on Unix, whichever the platform joined them
/// with, so that a path is named the same however it was made, as `test/<name>` is.
pub fn path_string(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) { path.replace('\\', "/") } else { path.into_owned() }
}
