//! The build graph: files, and the edges which produce them.

use std::time::Duration;

use crate::Error;

/// A file, or the name of an alias, in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

/// An edge in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeId(pub(crate) u32);

/// A program to run, with its arguments, environment, working directory and where its stderr goes. No shell reads
/// them, so they need no quoting. A command which needs a shell runs one itself, as `sh -c`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Exec {
    /// The program. One with a `/` is a path relative to the working directory, and any other is found in `PATH`.
    pub program: String,
    pub args: Vec<String>,
    /// Environment variables the program is given, beside those it inherits.
    pub env: Vec<(String, String)>,
    /// The working directory, relative to the build's directory, or `None` for the build's directory itself.
    pub dir: Option<String>,
    /// A file, relative to the build's directory, which stderr is written to rather than captured with stdout.
    pub stderr: Option<String>,
}

impl Exec {
    /// Returns the program as a command line for `sh`, to show it.
    #[must_use]
    pub fn command_line(&self) -> String {
        use std::fmt::Write as _;
        let mut line = String::new();
        if let Some(dir) = &self.dir {
            let _ = write!(line, "cd {} && ", shell_quote(dir));
        }
        for (name, value) in &self.env {
            let _ = write!(line, "{name}={} ", shell_quote(value));
        }
        let words = std::iter::once(&self.program).chain(&self.args);
        line.push_str(&words.map(|word| shell_quote(word)).collect::<Vec<_>>().join(" "));
        if let Some(stderr) = &self.stderr {
            let _ = write!(line, " 2> {}", shell_quote(stderr));
        }
        line
    }
}

/// Quote a word for `sh`, unless every character of it is known to be safe.
fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_+-./=:,@%".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Whether an edge produces its outputs by running a program, or only names other files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EdgeKind {
    /// An alias, which runs nothing, and is up to date once its inputs are.
    #[default]
    Alias,
    Command(Exec),
}

/// What an edge runs alongside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pool {
    /// The edge runs alongside any other, with its output captured.
    #[default]
    Default,
    /// The edge runs alone, with the terminal.
    Console,
    /// The edge runs with nothing else running, with its output captured, as a test which times what it runs, or
    /// which shares files with others, must.
    Serial,
}

/// An edge to add to a [`Graph`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    /// What is shown while the edge runs.
    pub description: String,
    /// The files the edge writes. The first names the edge, and is the key of its record.
    pub outputs: Vec<NodeId>,
    /// The files the edge reads, which must exist: a missing one which no edge produces is an error before anything
    /// runs.
    pub inputs: Vec<NodeId>,
    /// The files the edge depends on. Like inputs, a change to one runs the edge, but they may be missing: a missing
    /// one which no edge produces runs the edge, which reports the error itself if it still needs the file. A
    /// compiler whose dependency was removed along with its use, for example, does not.
    pub dependencies: Vec<NodeId>,
    /// The files which are built before the edge, but which it neither reads nor depends on.
    pub after: Vec<NodeId>,
    /// The depfile the action writes, in gcc's format, listing the inputs it discovered.
    pub depfile: Option<String>,
    pub pool: Pool,
    /// Whether the edge generates the graph, which is brought up to date before anything else.
    pub generator: bool,
    /// Whether the edge's outputs are stamped again once it has run, so that an edge which depends only on outputs
    /// that did not change is not run.
    pub restat: bool,
    /// Whether the edge's files are stamped by their contents, rather than their time and size. The edge then does
    /// not run when a file is changed back or rewritten unchanged, and neither does what depends on outputs it
    /// rewrites unchanged.
    pub hashed: bool,
    /// If set, the edge's results are kept in a [`Cache`](crate::cache::Cache), and fetched from it rather than run
    /// again. The string identifies everything the action depends on other than its inputs, such as its toolchain,
    /// and is part of the cache key. A cached edge's files are stamped by their contents, like a hashed edge's.
    pub cache: Option<String>,
    /// The inherited environment variables the action reads, beside those it sets. Their values are inputs of the
    /// edge, so it runs again when one changes. A cached edge runs in a hermetic environment, and inherits only these
    /// and the variables which locate tools, [`crate::exec::Options::locating`].
    pub pass_env: Vec<String>,
    /// Which workers a cached edge may run on.
    pub placement: Placement,
    /// A program which writes the edge's depfile without doing the edge's work, such as rustc with
    /// `--emit=dep-info` alone. It finds the inputs of an edge which has not run, so that it can be sent to a worker.
    pub discover: Option<Exec>,
    /// Whether the edge runs on every build which wants it, as a test does, whose results depend on more than it
    /// declares. Such an edge is never up to date, and is neither cached nor sent to a worker, whatever `cache` says.
    pub always: bool,
    /// How long the edge may run before it is killed, with what it started, and fails.
    pub timeout: Option<Duration>,
    /// Outputs which the action writes, and reports, before it finishes, such as a crate's metadata, which rustc
    /// reports with `--json=artifacts`. Edges which need only these outputs start as soon as they are written. An
    /// edge with early outputs must print what rustc prints with `--error-format=json`; the rendered diagnostics in
    /// it are shown.
    pub early: Vec<NodeId>,
}

/// Which workers a cached edge may run on, beside those whose toolchain has the edge's identity, or which an edge
/// that runs only on workers runs on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Whether it may run with the same release of its toolchain on another host, as what it writes does not depend
    /// on the host.
    pub any_host: bool,
    /// The target whose standard library it needs from the toolchain, if any.
    pub target: Option<String>,
    /// The tags a worker must have.
    pub tags: Vec<String>,
    /// For an edge which runs only on workers, never here, the target that a worker's host must run: it is sent to a
    /// worker whose host is that target, or, if the program it runs is linked statically, any Linux of the same
    /// architecture. It runs there without a toolchain, with the worker's own tools, and for what it prints and
    /// whether it succeeds: its outputs are not fetched. It fails if no worker runs it.
    pub runs: Option<String>,
}

/// The lists of nodes of an edge, in the order they are stored.
const OUTPUTS: usize = 0;
const INPUTS: usize = 1;
const DEPENDENCIES: usize = 2;
const AFTER: usize = 3;
/// The number of lists of nodes of an edge.
pub(crate) const LISTS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeData {
    kind: EdgeKind,
    description: String,
    depfile: Option<String>,
    pool: Pool,
    generator: bool,
    restat: bool,
    hashed: bool,
    cache: Option<String>,
    pass_env: Vec<String>,
    placement: Placement,
    discover: Option<Exec>,
    early: Vec<NodeId>,
    always: bool,
    timeout: Option<Duration>,
    /// The start and end of each list of nodes in [`Graph::ids`].
    lists: [(u32, u32); LISTS],
}

/// A build graph.
///
/// Paths are interned, so each path is one node, and edges are stored in flat arrays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    paths: Vec<String>,
    index: crate::hash::Map<String, NodeId>,
    /// The edge which produces each node, if one does.
    producers: Vec<Option<EdgeId>>,
    edges: Vec<EdgeData>,
    ids: Vec<NodeId>,
    defaults: Vec<NodeId>,
}

impl Graph {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Reserve room for as many more nodes.
    pub(crate) fn reserve(&mut self, nodes: usize) {
        self.paths.reserve(nodes);
        self.index.reserve(nodes);
        self.producers.reserve(nodes);
    }
    /// Returns the node of a path, adding it if it is not in the graph.
    ///
    /// # Panics
    ///
    /// If the graph has as many nodes as a `u32` can count.
    pub fn file(&mut self, path: &str) -> NodeId {
        if let Some(&id) = self.index.get(path) {
            return id;
        }
        let id = NodeId(u32::try_from(self.paths.len()).expect("too many nodes"));
        self.paths.push(path.into());
        self.index.insert(path.into(), id);
        self.producers.push(None);
        id
    }
    /// Returns the node of a path, if it is in the graph.
    #[must_use]
    pub fn node(&self, path: &str) -> Option<NodeId> {
        self.index.get(path).copied()
    }
    #[must_use]
    pub fn path(&self, node: NodeId) -> &str {
        &self.paths[node.0 as usize]
    }
    /// Returns the number of nodes.
    #[must_use]
    pub fn nodes(&self) -> usize {
        self.paths.len()
    }
    /// Returns the edge which produces a node, if one does.
    #[must_use]
    pub fn producer(&self, node: NodeId) -> Option<EdgeId> {
        self.producers[node.0 as usize]
    }
    #[must_use]
    pub fn edge(&self, edge: EdgeId) -> EdgeRef<'_> {
        EdgeRef { graph: self, data: &self.edges[edge.0 as usize] }
    }
    // clippy before 1.99 asks for `must_use`, which from 1.99 it finds is already the iterator's
    #[must_use]
    #[allow(clippy::double_must_use)]
    pub fn edges(&self) -> impl ExactSizeIterator<Item = EdgeRef<'_>> {
        self.edges.iter().map(|data| EdgeRef { graph: self, data })
    }
    /// The targets which are built when none are requested.
    #[must_use]
    pub fn defaults(&self) -> &[NodeId] {
        &self.defaults
    }
    /// Add an edge.
    ///
    /// # Errors
    ///
    /// If one of its outputs is produced by another edge, or is listed twice.
    ///
    /// # Panics
    ///
    /// If the graph has as many edges, or nodes in their lists, as a `u32` can count, or a node is not in the graph.
    pub fn add_edge(&mut self, mut edge: Edge) -> Result<EdgeId, Error> {
        let take = std::mem::take::<Vec<NodeId>>;
        let (outputs, inputs) = (take(&mut edge.outputs), take(&mut edge.inputs));
        let (dependencies, after) = (take(&mut edge.dependencies), take(&mut edge.after));
        self.add_edge_with(edge, [&outputs, &inputs, &dependencies, &after])
    }
    /// Add an edge whose outputs, inputs, dependencies and `after` nodes are given as slices, in that order, rather
    /// than in the edge. Loading a graph uses this to avoid allocating four vectors for each edge.
    pub(crate) fn add_edge_with(&mut self, edge: Edge, nodes: [&[NodeId]; LISTS]) -> Result<EdgeId, Error> {
        let id = EdgeId(u32::try_from(self.edges.len()).expect("too many edges"));
        let outputs = nodes[OUTPUTS].iter();
        for (i, &output) in outputs.clone().enumerate() {
            let twice = outputs.clone().take(i).any(|&other| other == output);
            if twice || self.producers[output.0 as usize].is_some() {
                return Err(Error::DuplicateOutput(self.path(output).into()));
            }
        }
        for &output in outputs {
            self.producers[output.0 as usize] = Some(id);
        }
        let mut lists = [(0, 0); LISTS];
        for (list, nodes) in lists.iter_mut().zip(nodes) {
            let start = u32::try_from(self.ids.len()).expect("too many nodes in edges");
            for node in nodes {
                assert!((node.0 as usize) < self.paths.len(), "node {node:?} is not in the graph");
            }
            self.ids.extend_from_slice(nodes);
            *list = (start, u32::try_from(self.ids.len()).expect("too many nodes in edges"));
        }
        self.edges.push(EdgeData {
            kind: edge.kind,
            description: edge.description,
            depfile: edge.depfile,
            pool: edge.pool,
            generator: edge.generator,
            restat: edge.restat,
            hashed: edge.hashed,
            cache: edge.cache,
            pass_env: edge.pass_env,
            placement: edge.placement,
            discover: edge.discover,
            early: edge.early,
            always: edge.always,
            timeout: edge.timeout,
            lists,
        });
        Ok(id)
    }
    /// Add an alias, which builds its targets.
    ///
    /// # Errors
    ///
    /// If the name is produced by another edge.
    pub fn add_alias(&mut self, name: &str, targets: &[NodeId]) -> Result<EdgeId, Error> {
        let name = self.file(name);
        self.add_edge(Edge { outputs: vec![name], inputs: targets.to_vec(), ..Edge::default() })
    }
    /// Add to the targets which are built when none are requested.
    pub fn add_default(&mut self, targets: &[NodeId]) {
        self.defaults.extend_from_slice(targets);
    }
    /// Check that the graph has no cycles.
    ///
    /// # Errors
    ///
    /// With the paths along the first cycle found.
    pub fn validate(&self) -> Result<(), Error> {
        const UNVISITED: u8 = 0;
        const VISITING: u8 = 1;
        const DONE: u8 = 2;
        let mut state = vec![UNVISITED; self.edges.len()];
        // each entry is an edge, the output it was reached by, and the next of its inputs to visit
        let mut stack: Vec<(usize, NodeId, usize)> = Vec::new();
        for start in 0..self.edges.len() {
            if state[start] != UNVISITED {
                continue;
            }
            state[start] = VISITING;
            let (from, to) = self.edges[start].lists[OUTPUTS];
            let first = self.ids[from as usize..to as usize].first().copied().unwrap_or(NodeId(0));
            stack.push((start, first, 0));
            while let Some(&mut (edge, _, ref mut next)) = stack.last_mut() {
                let inputs = self.inputs(edge);
                let Some(&input) = inputs.get(*next) else {
                    state[edge] = DONE;
                    stack.pop();
                    continue;
                };
                *next += 1;
                let Some(producer) = self.producer(input) else {
                    continue;
                };
                let producer = producer.0 as usize;
                match state[producer] {
                    UNVISITED => {
                        state[producer] = VISITING;
                        stack.push((producer, input, 0));
                    },
                    VISITING => {
                        let from = stack.iter().position(|&(edge, _, _)| edge == producer).unwrap_or(0);
                        let mut paths: Vec<String> =
                            stack[from..].iter().map(|&(_, node, _)| self.path(node).to_string()).collect();
                        paths.push(self.path(input).to_string());
                        return Err(Error::Cycle(paths));
                    },
                    _ => (),
                }
            }
        }
        Ok(())
    }
    /// What an edge needs built first: its inputs, its dependencies and what it is built after. The three lists are
    /// stored next to each other, so this is one slice.
    fn inputs(&self, edge: usize) -> &[NodeId] {
        let lists = &self.edges[edge].lists;
        &self.ids[lists[INPUTS].0 as usize..lists[AFTER].1 as usize]
    }
}

/// An edge of a [`Graph`].
#[derive(Clone, Copy)]
pub struct EdgeRef<'a> {
    graph: &'a Graph,
    data: &'a EdgeData,
}
impl<'a> EdgeRef<'a> {
    #[must_use]
    pub fn kind(&self) -> &'a EdgeKind {
        &self.data.kind
    }
    #[must_use]
    pub fn description(&self) -> &'a str {
        &self.data.description
    }
    #[must_use]
    pub fn outputs(&self) -> &'a [NodeId] {
        self.list(OUTPUTS)
    }
    #[must_use]
    pub fn inputs(&self) -> &'a [NodeId] {
        self.list(INPUTS)
    }
    #[must_use]
    pub fn dependencies(&self) -> &'a [NodeId] {
        self.list(DEPENDENCIES)
    }
    #[must_use]
    pub fn after(&self) -> &'a [NodeId] {
        self.list(AFTER)
    }
    #[must_use]
    pub fn depfile(&self) -> Option<&'a str> {
        self.data.depfile.as_deref()
    }
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.data.pool
    }
    #[must_use]
    pub fn generator(&self) -> bool {
        self.data.generator
    }
    #[must_use]
    pub fn restat(&self) -> bool {
        self.data.restat
    }
    /// Whether the edge's files are stamped by their contents, which they are if it is hashed or cached.
    #[must_use]
    pub fn hashed(&self) -> bool {
        self.data.hashed || self.cache().is_some()
    }
    /// The edge's own `hashed` flag, which does not include being cached.
    pub(crate) fn hashed_flag(&self) -> bool {
        self.data.hashed
    }
    /// The environment variables the edge's action reads from those it inherits.
    #[must_use]
    pub fn pass_env(&self) -> &'a [String] {
        &self.data.pass_env
    }
    /// Whether the edge runs in a hermetic environment, as a cached edge does: with only the variables it sets, those
    /// it passes on and those which locate tools. A variable outside its cache key then cannot change what it
    /// produces.
    #[must_use]
    pub fn hermetic(&self) -> bool {
        self.cache().is_some()
    }
    /// The edge's cache identity, if its results are cached. See [`Edge::cache`]. An edge which runs [`always`] has
    /// none.
    ///
    /// [`always`]: Edge::always
    #[must_use]
    pub fn cache(&self) -> Option<&'a str> {
        self.data.cache.as_deref().filter(|_| !self.data.always)
    }
    /// The cache identity the edge was given, whether or not it runs [`always`](Edge::always), as it is saved.
    pub(crate) fn given_cache(&self) -> Option<&'a str> {
        self.data.cache.as_deref()
    }
    /// Whether the edge runs on every build which wants it. See [`Edge::always`].
    #[must_use]
    pub fn always(&self) -> bool {
        self.data.always
    }
    /// How long the edge may run before it is killed, if it has a limit.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        self.data.timeout
    }
    /// Which workers the edge may run on, if it is cached.
    #[must_use]
    pub fn placement(&self) -> &'a Placement {
        &self.data.placement
    }
    /// The program which writes the edge's depfile without running the edge, if it has one.
    #[must_use]
    pub fn discover(&self) -> Option<&'a Exec> {
        self.data.discover.as_ref()
    }
    /// The outputs the edge reports before it finishes, as rustc does, if it has any.
    #[must_use]
    pub fn early(&self) -> &'a [NodeId] {
        &self.data.early
    }
    /// Returns the edge as it would be added to a graph.
    #[must_use]
    pub fn to_edge(&self) -> Edge {
        Edge {
            kind: self.data.kind.clone(),
            description: self.data.description.clone(),
            outputs: self.outputs().to_vec(),
            inputs: self.inputs().to_vec(),
            dependencies: self.dependencies().to_vec(),
            after: self.after().to_vec(),
            depfile: self.data.depfile.clone(),
            pool: self.data.pool,
            generator: self.data.generator,
            restat: self.data.restat,
            hashed: self.data.hashed,
            cache: self.data.cache.clone(),
            pass_env: self.data.pass_env.clone(),
            placement: self.data.placement.clone(),
            discover: self.data.discover.clone(),
            early: self.data.early.clone(),
            always: self.data.always,
            timeout: self.data.timeout,
        }
    }
    fn list(&self, list: usize) -> &'a [NodeId] {
        let (start, end) = self.data.lists[list];
        &self.graph.ids[start as usize..end as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(graph: &mut Graph, outputs: &[&str], inputs: &[&str]) -> Result<EdgeId, Error> {
        let outputs = outputs.iter().map(|path| graph.file(path)).collect();
        let inputs = inputs.iter().map(|path| graph.file(path)).collect();
        let kind = EdgeKind::Command(Exec { program: "true".into(), ..Exec::default() });
        graph.add_edge(Edge { kind, outputs, inputs, ..Edge::default() })
    }

    #[test]
    fn interns_paths() {
        let mut graph = Graph::new();
        let a = graph.file("a");
        assert_eq!(graph.file("b"), NodeId(1));
        assert_eq!(graph.file("a"), a);
        assert_eq!(graph.node("b"), Some(NodeId(1)));
        assert_eq!(graph.node("c"), None);
        assert_eq!(graph.path(a), "a");
        assert_eq!(graph.nodes(), 2);
    }

    #[test]
    fn records_producers() {
        let mut graph = Graph::new();
        let edge = command(&mut graph, &["out"], &["in"]).unwrap();
        assert_eq!(graph.producer(graph.node("out").unwrap()), Some(edge));
        assert_eq!(graph.producer(graph.node("in").unwrap()), None);
        let edge = graph.edge(edge);
        assert_eq!(edge.outputs(), &[NodeId(0)]);
        assert_eq!(edge.inputs(), &[NodeId(1)]);
        assert_eq!(edge.dependencies(), &[] as &[NodeId]);
    }

    #[test]
    fn refuses_duplicate_outputs() {
        let mut graph = Graph::new();
        command(&mut graph, &["out"], &[]).unwrap();
        assert!(matches!(command(&mut graph, &["out"], &[]), Err(Error::DuplicateOutput(path)) if path == "out"));
        assert!(matches!(command(&mut graph, &["x", "x"], &[]), Err(Error::DuplicateOutput(path)) if path == "x"));
        assert!(matches!(graph.add_alias("out", &[]), Err(Error::DuplicateOutput(_))));
        // the refused edges are not added, and do not produce their other outputs
        assert_eq!(graph.edges().len(), 1);
        assert_eq!(graph.producer(graph.node("x").unwrap()), None);
    }

    #[test]
    fn finds_cycles() {
        let mut graph = Graph::new();
        command(&mut graph, &["a"], &["b"]).unwrap();
        command(&mut graph, &["b"], &["c", "src"]).unwrap();
        assert!(graph.validate().is_ok());
        command(&mut graph, &["c"], &["a"]).unwrap();
        match graph.validate() {
            Err(Error::Cycle(paths)) => assert_eq!(paths, ["a", "b", "c", "a"]),
            result => panic!("{result:?}"),
        }
    }
}
