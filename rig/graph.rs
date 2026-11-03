//! The build graph: files, and the edges which produce them.

use std::collections::HashMap;

use crate::Error;

/// A file, or the name of an alias, in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

/// An edge in a [`Graph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EdgeId(pub(crate) u32);

/// What an edge runs to produce its outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// A command line, run by `sh -c`.
    Shell(String),
}

/// Whether an edge produces its outputs by running an action, or only names other files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EdgeKind {
    /// An alias, as ninja's `phony`, which runs nothing, and is up to date once its inputs are.
    #[default]
    Alias,
    Command(Action),
}

/// What an edge runs alongside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pool {
    /// The edge runs alongside any other, with its output captured.
    #[default]
    Default,
    /// The edge runs alone, with the terminal, as ninja's `console` pool.
    Console,
}

/// An edge to add to a [`Graph`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    /// What is shown while the edge runs.
    pub description: String,
    pub outputs: Vec<NodeId>,
    /// Outputs which are not part of `$out`.
    pub implicit_outputs: Vec<NodeId>,
    pub inputs: Vec<NodeId>,
    /// Inputs which trigger a rebuild but are not part of `$in`.
    pub implicit: Vec<NodeId>,
    /// Inputs which must be built first but do not trigger a rebuild.
    pub order_only: Vec<NodeId>,
    /// The depfile the action writes, in the form gcc writes, of the inputs it discovered.
    pub depfile: Option<String>,
    pub pool: Pool,
    /// Whether the edge generates the graph, which is brought up to date before anything else.
    pub generator: bool,
    /// Whether the edge's outputs are checked again once it has run, so that what depends on an output which did not
    /// change is not run.
    pub restat: bool,
}

/// The lists of nodes of an edge, in the order they are stored.
const OUTPUTS: usize = 0;
const IMPLICIT_OUTPUTS: usize = 1;
const INPUTS: usize = 2;
const IMPLICIT: usize = 3;
const ORDER_ONLY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeData {
    kind: EdgeKind,
    description: String,
    depfile: Option<String>,
    pool: Pool,
    generator: bool,
    restat: bool,
    /// The start and end of each list of nodes in [`Graph::ids`].
    lists: [(u32, u32); 5],
}

/// A build graph.
///
/// Paths are interned, so each path is one node, and edges are stored in flat arrays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    paths: Vec<String>,
    index: HashMap<String, NodeId>,
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
    pub fn add_edge(&mut self, edge: Edge) -> Result<EdgeId, Error> {
        let id = EdgeId(u32::try_from(self.edges.len()).expect("too many edges"));
        let outputs = edge.outputs.iter().chain(&edge.implicit_outputs);
        for (i, &output) in outputs.clone().enumerate() {
            let twice = outputs.clone().take(i).any(|&other| other == output);
            if twice || self.producers[output.0 as usize].is_some() {
                return Err(Error::DuplicateOutput(self.path(output).into()));
            }
        }
        for &output in outputs {
            self.producers[output.0 as usize] = Some(id);
        }
        let mut lists = [(0, 0); 5];
        let nodes = [&edge.outputs, &edge.implicit_outputs, &edge.inputs, &edge.implicit, &edge.order_only];
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
    /// All of the inputs of an edge, explicit, implicit and order-only, which are stored together.
    fn inputs(&self, edge: usize) -> &[NodeId] {
        let lists = &self.edges[edge].lists;
        &self.ids[lists[INPUTS].0 as usize..lists[ORDER_ONLY].1 as usize]
    }
    /// Returns the first difference between two graphs, if they differ, as a description of it.
    #[must_use]
    pub fn diff(&self, other: &Graph) -> Option<String> {
        let edges = self.edges().zip(other.edges());
        for (index, (lhs, rhs)) in edges.enumerate() {
            let (lhs, rhs) = (lhs.to_edge_paths(), rhs.to_edge_paths());
            if lhs != rhs {
                return Some(format!("edge {index} differs:\n{lhs:#?}\nrather than\n{rhs:#?}"));
            }
        }
        if self.edges.len() != other.edges.len() {
            return Some(format!("{} edges rather than {}", self.edges.len(), other.edges.len()));
        }
        let defaults = |graph: &Graph| graph.defaults.iter().map(|&node| graph.path(node).to_string()).collect::<Vec<_>>();
        if defaults(self) != defaults(other) {
            return Some(format!("defaults {:?} rather than {:?}", defaults(self), defaults(other)));
        }
        if self.paths != other.paths {
            let first = self.paths.iter().zip(&other.paths).position(|(lhs, rhs)| lhs != rhs);
            return Some(match first {
                Some(index) => format!("node {index} is {:?} rather than {:?}", self.paths[index], other.paths[index]),
                None => format!("{} nodes rather than {}", self.paths.len(), other.paths.len()),
            });
        }
        None
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
    pub fn implicit_outputs(&self) -> &'a [NodeId] {
        self.list(IMPLICIT_OUTPUTS)
    }
    #[must_use]
    pub fn inputs(&self) -> &'a [NodeId] {
        self.list(INPUTS)
    }
    #[must_use]
    pub fn implicit(&self) -> &'a [NodeId] {
        self.list(IMPLICIT)
    }
    #[must_use]
    pub fn order_only(&self) -> &'a [NodeId] {
        self.list(ORDER_ONLY)
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
    /// Returns the edge as it would be added to a graph.
    #[must_use]
    pub fn to_edge(&self) -> Edge {
        Edge {
            kind: self.data.kind.clone(),
            description: self.data.description.clone(),
            outputs: self.outputs().to_vec(),
            implicit_outputs: self.implicit_outputs().to_vec(),
            inputs: self.inputs().to_vec(),
            implicit: self.implicit().to_vec(),
            order_only: self.order_only().to_vec(),
            depfile: self.data.depfile.clone(),
            pool: self.data.pool,
            generator: self.data.generator,
            restat: self.data.restat,
        }
    }
    fn list(&self, list: usize) -> &'a [NodeId] {
        let (start, end) = self.data.lists[list];
        &self.graph.ids[start as usize..end as usize]
    }
    /// The edge with the paths of its nodes, rather than their IDs, which are compared between graphs.
    fn to_edge_paths(self) -> EdgePaths<'a> {
        let paths = |list| self.list(list).iter().map(|&node| self.graph.path(node)).collect();
        let data = self.data;
        EdgePaths {
            kind: &data.kind,
            description: &data.description,
            depfile: data.depfile.as_deref(),
            pool: data.pool,
            generator: data.generator,
            restat: data.restat,
            nodes: [paths(OUTPUTS), paths(IMPLICIT_OUTPUTS), paths(INPUTS), paths(IMPLICIT), paths(ORDER_ONLY)],
        }
    }
}

/// An edge with the paths of its nodes, in the order of its lists, as two graphs are compared.
#[derive(Debug, PartialEq)]
struct EdgePaths<'a> {
    kind: &'a EdgeKind,
    description: &'a str,
    depfile: Option<&'a str>,
    pool: Pool,
    generator: bool,
    restat: bool,
    nodes: [Vec<&'a str>; 5],
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(graph: &mut Graph, outputs: &[&str], inputs: &[&str]) -> Result<EdgeId, Error> {
        let outputs = outputs.iter().map(|path| graph.file(path)).collect();
        let inputs = inputs.iter().map(|path| graph.file(path)).collect();
        graph.add_edge(Edge { kind: EdgeKind::Command(Action::Shell("true".into())), outputs, inputs, ..Edge::default() })
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
        assert!(edge.implicit().is_empty());
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

    #[test]
    fn finds_differences() {
        let mut lhs = Graph::new();
        command(&mut lhs, &["a"], &["b"]).unwrap();
        let mut rhs = lhs.clone();
        assert_eq!(lhs.diff(&rhs), None);
        rhs.add_default(&[NodeId(0)]);
        assert!(lhs.diff(&rhs).unwrap().starts_with("defaults"));
        command(&mut rhs, &["c"], &[]).unwrap();
        assert_eq!(lhs.diff(&rhs).unwrap(), "1 edges rather than 2");
    }
}
