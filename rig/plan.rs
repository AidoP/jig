//! Planning a build: which edges must run to bring targets up to date, and why.
//!
//! An edge must run if it has no record, its outputs or command have changed, one of its outputs is missing or is not
//! what the edge produced, the inputs and dependencies it declares have changed, one of them is produced by an edge
//! which must run, or the stamp of one of them, or of an input discovered from its depfile, differs from the one
//! recorded. An input which is missing, and which no edge produces, is an error, while a dependency or a discovered
//! input which is missing runs the edge, which is then up to date until it is there again. What an edge is built
//! after is built first, but does not make it run. The command of a generator edge may change without it running, as
//! the generator writes the graph which has it.
//!
//! An alias is out of date if an input of it is produced by an edge which must run, or if it has no inputs and is not a
//! file. An edge which takes an alias as an input takes the inputs of the alias instead, so that it runs when one of
//! them changes.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::Error;
use crate::depfile::Discovered;
use crate::graph::{EdgeId, EdgeKind, EdgeRef, Exec, Graph, NodeId};
use crate::stamp::{Stamp, Stamps};
use crate::state::{Record, State};

/// Why an edge must run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The edge has not run, or has no record of it.
    NoRecord,
    /// The outputs of the edge are not those it last ran with.
    OutputsChanged,
    CommandChanged,
    OutputMissing(String),
    /// An output is not what the edge produced, as it has been changed since.
    OutputModified(String),
    /// The inputs the edge declares are not those it last ran with.
    InputsChanged,
    /// An input is produced by an edge which must run.
    InputRebuilt(String),
    InputChanged(String),
    /// A dependency which the edge had when it last ran is missing, and no edge produces it.
    DependencyMissing(String),
    /// An input which the edge discovered, as its depfile lists, has changed.
    DiscoveredChanged(String),
    /// An environment variable which the edge read, as its depfile lists, has changed, or been set or unset.
    EnvChanged(String),
    /// An alias without inputs, which is not a file, which is always out of date.
    Forced,
}
impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::NoRecord => write!(f, "it has no record of running"),
            Self::OutputsChanged => write!(f, "its outputs have changed"),
            Self::CommandChanged => write!(f, "its command has changed"),
            Self::OutputMissing(path) => write!(f, "output {path:?} is missing"),
            Self::OutputModified(path) => write!(f, "output {path:?} has been modified since it was built"),
            Self::InputsChanged => write!(f, "its inputs have changed"),
            Self::InputRebuilt(path) => write!(f, "input {path:?} is built again"),
            Self::InputChanged(path) => write!(f, "input {path:?} has changed"),
            Self::DependencyMissing(path) => write!(f, "dependency {path:?} is missing"),
            Self::DiscoveredChanged(path) => write!(f, "discovered input {path:?} has changed"),
            Self::EnvChanged(name) => write!(f, "environment variable {name:?} has changed"),
            Self::Forced => write!(f, "it is an alias without inputs, which is always out of date"),
        }
    }
}

/// The edges which must run to bring targets up to date, in an order in which each runs after those it depends on.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub edges: Vec<(EdgeId, Reason)>,
}
impl Plan {
    /// Whether the targets are up to date, as no edge must run.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Returns the hash of an edge's action, which it runs again when it changes, with what identifies it in the cache if
/// it is cached, as that identifies what else the action depends on, such as its toolchain, and the variables it
/// passes on, as which it inherits are part of what it does.
#[must_use]
pub fn action_hash(edge: EdgeRef) -> u64 {
    // each hash is of the bytes it would be of if they were together, as they are hashed in turn, so that a large
    // graph's hashes are found without copying each action
    let mut hash = match edge.kind() {
        EdgeKind::Alias => 0,
        EdgeKind::Command(action) => {
            let mut fnv = Fnv::new();
            parts_of(action, &mut |bytes| fnv.write(bytes));
            fnv.0
        },
    };
    if let Some(identity) = edge.cache() {
        let mut fnv = Fnv::new();
        fnv.write(&hash.to_le_bytes());
        fnv.write(b"\0cache");
        fnv.write(identity.as_bytes());
        hash = fnv.0;
    }
    if !edge.pass_env().is_empty() {
        let mut fnv = Fnv::new();
        fnv.write(&hash.to_le_bytes());
        fnv.write(b"\0pass-env");
        for name in edge.pass_env() {
            fnv.write(&name.len().to_le_bytes());
            fnv.write(name.as_bytes());
        }
        hash = fnv.0;
    }
    hash
}

/// The 64-bit FNV-1a hash of bytes given in parts, as [`crate::state::fnv1a`] is of them together.
struct Fnv(u64);
impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0 = bytes.iter().fold(self.0, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
    }
}

/// Returns an edge's action as bytes, which differ whenever it does: each part of its program with its length before
/// it, so that parts which run together differ.
#[must_use]
pub fn action_bytes(edge: EdgeRef) -> Vec<u8> {
    match edge.kind() {
        EdgeKind::Alias => Vec::new(),
        EdgeKind::Command(action) => bytes_of(action),
    }
}

/// Returns an edge's action as bytes, as [`action_bytes`] does, with a marker in place of each occurrence of the
/// build's directory, which is absolute, so that the action is the same in a build in another directory, as for the
/// key of a cache which builds in several share.
///
/// A command given the build's directory, such as to remap it in what it writes, then has its results shared with
/// builds elsewhere, so it should write nothing which depends on it.
#[must_use]
pub fn relocated_action_bytes(edge: EdgeRef, dir: &str) -> Vec<u8> {
    match edge.kind() {
        EdgeKind::Alias => Vec::new(),
        EdgeKind::Command(action) => bytes_of(&relocated(action, dir)),
    }
}

/// What stands for the build's directory in an action which is relocated.
pub(crate) const BUILD_DIR: &str = "\0build-dir";

/// Returns an action with the build's directory, absolute, replaced wherever it is by [`BUILD_DIR`], or as it is if
/// the directory is empty, as it is when it is not known.
pub(crate) fn relocated(exec: &Exec, dir: &str) -> Exec {
    let relocate = |string: &str| if dir.is_empty() { string.to_string() } else { string.replace(dir, BUILD_DIR) };
    Exec {
        program: relocate(&exec.program),
        args: exec.args.iter().map(|arg| relocate(arg)).collect(),
        env: exec.env.iter().map(|(name, value)| (name.clone(), relocate(value))).collect(),
        dir: exec.dir.as_deref().map(relocate),
        stderr: exec.stderr.as_deref().map(relocate),
    }
}

fn bytes_of(action: &Exec) -> Vec<u8> {
    let mut bytes = Vec::new();
    parts_of(action, &mut |part| bytes.extend_from_slice(part));
    bytes
}

/// Gives the bytes of an action, as [`bytes_of`] returns them, in parts, in turn.
fn parts_of(exec: &Exec, write: &mut dyn FnMut(&[u8])) {
    let mut part = |part: &str| {
        write(&part.len().to_le_bytes());
        write(part.as_bytes());
    };
    part(&exec.program);
    exec.args.iter().for_each(|arg| part(arg));
    part("\0env");
    exec.env.iter().for_each(|(name, value)| {
        part(name);
        part(value);
    });
    part(exec.dir.as_deref().unwrap_or("\0"));
    part(exec.stderr.as_deref().unwrap_or("\0"));
}

/// Returns the inputs and dependencies an edge declares, with those of each alias in place of the alias.
#[must_use]
pub fn declared_inputs(graph: &Graph, edge: EdgeRef) -> Vec<NodeId> {
    let mut inputs = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<NodeId> = edge.inputs().iter().chain(edge.dependencies()).rev().copied().collect();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        let alias = graph.producer(node).map(|edge| graph.edge(edge)).filter(|edge| *edge.kind() == EdgeKind::Alias);
        match alias {
            Some(alias) => stack.extend(alias.inputs().iter().chain(alias.dependencies()).rev()),
            None => inputs.push(node),
        }
    }
    inputs
}

/// Returns the stamps of the inputs an edge declares, as they are when it starts, to record once it has run, by their
/// contents if it is hashed, reusing the hashes of its last record.
pub fn input_stamps(graph: &Graph, state: &State, edge: EdgeId, stamps: &mut Stamps) -> Vec<(String, Stamp)> {
    let data = graph.edge(edge);
    let previous = state.get(first_output(graph, edge));
    let recorded = |path: &str| {
        let record = previous?;
        let (_, stamp) = record.inputs.iter().find(|(recorded, _)| recorded == path)?;
        Some((stamp, record.start))
    };
    let inputs = declared_inputs(graph, data);
    inputs.into_iter().map(|node| {
        let path = graph.path(node);
        (path.to_string(), stamps.node_as(node, data.hashed(), recorded(path)))
    }).collect()
}

/// Returns when the stamps of an edge's outputs and discovered inputs were taken, as it finished, in milliseconds since
/// the Unix epoch.
fn finished(record: &Record) -> u64 {
    record.start.saturating_add(u64::from(record.duration))
}

/// Returns the record of an edge which has run, from the stamps of its inputs when it started, and the inputs it
/// discovered.
///
/// The stamps of its outputs and discovered inputs are found again, as the edge may have changed them.
pub fn record(
    graph: &Graph,
    edge: EdgeId,
    inputs: Vec<(String, Stamp)>,
    discovered: &Discovered,
    stamps: &mut Stamps,
    start: SystemTime,
    duration: Duration,
) -> Record {
    let edge = graph.edge(edge);
    let mut outputs = Vec::new();
    for &output in edge.outputs() {
        stamps.forget(output);
        outputs.push((graph.path(output).to_string(), stamps.node_as(output, edge.hashed(), None)));
    }
    // the variables it passes on, then those it discovered, each once
    let mut names: Vec<&String> = edge.pass_env().iter().collect();
    for name in &discovered.env {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let env = names.into_iter().map(|name| (name.clone(), stamps.env(name))).collect();
    let discovered = discovered.inputs.iter().map(|path| (path.clone(), stamps.path_as(path, edge.hashed(), None)));
    let discovered = discovered.collect();
    let start = start.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    Record {
        start: u64::try_from(start.as_millis()).unwrap_or(u64::MAX),
        duration: u32::try_from(duration.as_millis()).unwrap_or(u32::MAX),
        action: action_hash(edge),
        outputs,
        inputs,
        discovered,
        env,
    }
}

/// Returns the targets which are built when none are requested: the defaults, or without them, every node which is
/// produced by an edge and is not an input of one.
#[must_use]
pub fn default_targets(graph: &Graph) -> Vec<NodeId> {
    if !graph.defaults().is_empty() {
        return graph.defaults().to_vec();
    }
    let mut used = vec![false; graph.nodes()];
    for edge in graph.edges() {
        for node in edge.inputs().iter().chain(edge.dependencies()).chain(edge.after()) {
            used[node.0 as usize] = true;
        }
    }
    let produced = graph.edges().flat_map(|edge| edge.outputs().iter());
    produced.copied().filter(|node| !used[node.0 as usize]).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Unvisited,
    Visiting,
    Clean,
    Dirty,
}

/// Plan the build of targets, given by their paths, or the default targets if none are given.
///
/// # Errors
///
/// If a target is not in the graph, or an input which no edge produces is missing, or the graph has a cycle.
pub fn plan<'g>(graph: &'g Graph, state: &State, stamps: &mut Stamps<'g>, targets: &[&str]) -> Result<Plan, Error> {
    let targets = target_nodes(graph, targets)?;
    // an output left on a worker is stamped as it was left while its file is missing
    stamps.extend_remote(state.remote_outputs());
    // the files the plan may need are stamped together first
    let (nodes, paths) = reachable(graph, state, &targets);
    stamps.prefetch(&nodes, &paths.iter().map(String::as_str).collect::<Vec<_>>());
    let status = vec![Status::Unvisited; graph.edges().len()];
    let mut planner = Planner { graph, state, stamps, status, plan: Plan::default() };
    for target in targets {
        match graph.producer(target) {
            Some(edge) => planner.visit(edge)?,
            None if planner.stamps.node(target).exists() => (),
            None => return Err(Error::MissingInput { input: graph.path(target).into(), needed_by: None }),
        }
    }
    Ok(planner.plan)
}

/// Returns the nodes which building targets may stamp: the targets, and the outputs and inputs of the edges which
/// produce them, and so on, and the paths which those edges discovered when they last ran which are not nodes.
fn reachable(graph: &Graph, state: &State, targets: &[NodeId]) -> (Vec<NodeId>, Vec<String>) {
    let mut seen = vec![false; graph.nodes()];
    let mut visited = vec![false; graph.edges().len()];
    let (mut nodes, mut paths) = (Vec::new(), Vec::new());
    let mut stack = targets.to_vec();
    while let Some(node) = stack.pop() {
        if std::mem::replace(&mut seen[node.0 as usize], true) {
            continue;
        }
        nodes.push(node);
        let Some(edge) = graph.producer(node) else {
            continue;
        };
        if std::mem::replace(&mut visited[edge.0 as usize], true) {
            continue;
        }
        let data = graph.edge(edge);
        let lists = [data.outputs(), data.inputs(), data.dependencies(), data.after()];
        stack.extend(lists.into_iter().flatten());
        for (path, _) in state.get(first_output(graph, edge)).into_iter().flat_map(|record| &record.discovered) {
            match graph.node(path) {
                Some(node) => stack.push(node),
                None => paths.push(path.clone()),
            }
        }
    }
    (nodes, paths)
}

struct Planner<'g, 's, 'b> {
    graph: &'g Graph,
    state: &'s State,
    stamps: &'b mut Stamps<'g>,
    status: Vec<Status>,
    plan: Plan,
}
impl Planner<'_, '_, '_> {
    /// Visit an edge, after the edges which produce its inputs, finding whether it must run.
    fn visit(&mut self, start: EdgeId) -> Result<(), Error> {
        if self.status[start.0 as usize] != Status::Unvisited {
            return Ok(());
        }
        // each entry is an edge, what is built first, of which the first must be there, and the next to visit
        let mut stack = vec![self.frame(start)];
        self.status[start.0 as usize] = Status::Visiting;
        while let Some((edge, inputs, required, next)) = stack.last_mut() {
            let (edge, required) = (*edge, *required);
            let data = self.graph.edge(edge);
            let Some(&input) = inputs.get(*next) else {
                stack.pop();
                let reason = self.evaluate(edge);
                self.status[edge.0 as usize] = if reason.is_some() { Status::Dirty } else { Status::Clean };
                if let Some(reason) = reason && *data.kind() != EdgeKind::Alias {
                    self.plan.edges.push((edge, reason));
                }
                continue;
            };
            let index = *next;
            *next += 1;
            let Some(producer) = self.graph.producer(input) else {
                // a dependency or a discovered input which is missing makes its edge run, rather than failing
                if index < required && !self.stamps.node(input).exists() {
                    let needed_by = data.outputs().first().map(|&node| self.graph.path(node).to_string());
                    return Err(Error::MissingInput { input: self.graph.path(input).into(), needed_by });
                }
                continue;
            };
            match self.status[producer.0 as usize] {
                Status::Unvisited => {
                    self.status[producer.0 as usize] = Status::Visiting;
                    stack.push(self.frame(producer));
                },
                Status::Visiting => {
                    let from = stack.iter().position(|&(edge, ..)| edge == producer).unwrap_or(0);
                    let paths = stack[from..].iter().map(|&(edge, ..)| self.first_output(edge));
                    let mut paths: Vec<String> = paths.collect();
                    paths.push(self.first_output(producer));
                    return Err(Error::Cycle(paths));
                },
                Status::Clean | Status::Dirty => (),
            }
        }
        Ok(())
    }
    /// Returns an edge to visit, with what is built first: its inputs and what it is built after, which must be there,
    /// as the count of them says, then its dependencies, and the inputs it discovered when it last ran which an edge
    /// produces, which may be missing.
    fn frame(&self, edge: EdgeId) -> (EdgeId, Vec<NodeId>, usize, usize) {
        let data = self.graph.edge(edge);
        let mut inputs: Vec<NodeId> = data.inputs().iter().chain(data.after()).copied().collect();
        let required = inputs.len();
        inputs.extend(data.dependencies());
        inputs.extend(self.discovered(edge));
        (edge, inputs, required, 0)
    }
    fn discovered(&self, edge: EdgeId) -> Vec<NodeId> {
        discovered_nodes(self.graph, self.state, edge)
    }
    fn first_output(&self, edge: EdgeId) -> String {
        first_output(self.graph, edge).to_string()
    }
    /// Returns why an edge must run, once the edges which produce its inputs have been visited, or `None` if it is up
    /// to date.
    fn evaluate(&mut self, id: EdgeId) -> Option<Reason> {
        let status = &self.status;
        reason(self.graph, self.state, self.stamps, id, &|producer| status[producer.0 as usize] == Status::Dirty)
    }
}

/// Returns the first output of an edge, which its record is kept by.
pub(crate) fn first_output(graph: &Graph, edge: EdgeId) -> &str {
    let edge = graph.edge(edge);
    let output = edge.outputs().iter().next();
    output.map_or("", |&node| graph.path(node))
}

/// Returns the nodes of targets, given by their paths, or the default targets if none are given.
///
/// # Errors
///
/// If a target is not a node of the graph.
pub fn target_nodes(graph: &Graph, targets: &[&str]) -> Result<Vec<NodeId>, Error> {
    if targets.is_empty() {
        return Ok(default_targets(graph));
    }
    let nodes = targets.iter().map(|&path| graph.node(path).ok_or_else(|| Error::UnknownTarget(path.into())));
    nodes.collect()
}

/// Returns the nodes an edge discovered when it last ran which an edge produces, which are built before it.
pub(crate) fn discovered_nodes(graph: &Graph, state: &State, edge: EdgeId) -> Vec<NodeId> {
    let record = state.get(first_output(graph, edge));
    let paths = record.into_iter().flat_map(|record| &record.discovered);
    let nodes = paths.filter_map(|(path, _)| graph.node(path));
    nodes.filter(|&node| graph.producer(node).is_some()).collect()
}

/// Returns the stamp an input was recorded with, as its hash is reused by: that which the edge which read it recorded,
/// or if it was left on a worker, and has been fetched since, that which the edge which left it recorded once it was,
/// with the time and size it was fetched with.
fn fetched_as<'a>(graph: &Graph, state: &'a State, path: &str, stamp: &'a Stamp) -> &'a Stamp {
    let Stamp::Remote { hash, .. } = stamp else {
        return stamp;
    };
    let producer = graph.node(path).and_then(|node| graph.producer(node));
    let record = producer.and_then(|edge| state.get(first_output(graph, edge)));
    let output = record.and_then(|record| record.outputs.iter().find(|(output, _)| output == path));
    match output {
        Some((_, fetched @ Stamp::Content { hash: fetched_hash, .. })) if fetched_hash == hash => fetched,
        _ => stamp,
    }
}

/// Returns why an edge must run, or `None` if it is up to date, where `rebuilt` is whether an edge which produces one
/// of its inputs runs, and so changes the input.
pub(crate) fn reason(
    graph: &Graph,
    state: &State,
    stamps: &mut Stamps,
    id: EdgeId,
    rebuilt: &dyn Fn(EdgeId) -> bool,
) -> Option<Reason> {
    let edge = graph.edge(id);
    let discovered = discovered_nodes(graph, state, id);
    let rebuilt = edge.inputs().iter().chain(edge.dependencies()).chain(&discovered).find(|&&input| {
        graph.producer(input).is_some_and(rebuilt)
    });
    if *edge.kind() == EdgeKind::Alias {
        if let Some(&input) = rebuilt {
            return Some(Reason::InputRebuilt(graph.path(input).into()));
        }
        let forced = edge.inputs().is_empty() && edge.dependencies().is_empty();
        let missing = edge.outputs().iter().any(|&output| !stamps.node(output).exists());
        return (forced && missing).then_some(Reason::Forced);
    }
    let outputs = edge.outputs().to_vec();
    let Some(record) = outputs.first().and_then(|&first| state.get(graph.path(first))) else {
        return Some(Reason::NoRecord);
    };
    let recorded = record.outputs.iter().map(|(path, _)| path.as_str());
    if !recorded.eq(outputs.iter().map(|&node| graph.path(node))) {
        return Some(Reason::OutputsChanged);
    }
    if !edge.generator() && record.action != action_hash(edge) {
        return Some(Reason::CommandChanged);
    }
    let hashed = edge.hashed();
    // a file which was left on a worker is compared by its contents once it is here, as is one which was fetched since,
    // whose edge recorded it by them then
    let hashed_as = |stamp: &Stamp| hashed || stamp.hash().is_some();
    for (&output, (path, stamp)) in outputs.iter().zip(&record.outputs) {
        let now = stamps.node_as(output, hashed_as(stamp), Some((stamp, finished(record))));
        if !now.exists() {
            return Some(Reason::OutputMissing(path.clone()));
        }
        if !now.same(*stamp) {
            return Some(Reason::OutputModified(path.clone()));
        }
    }
    let declared = declared_inputs(graph, edge);
    if !record.inputs.iter().map(|(path, _)| path.as_str()).eq(declared.iter().map(|&node| graph.path(node))) {
        return Some(Reason::InputsChanged);
    }
    if let Some(&input) = rebuilt {
        return Some(Reason::InputRebuilt(graph.path(input).into()));
    }
    for (&node, (path, stamp)) in declared.iter().zip(&record.inputs) {
        let recorded = fetched_as(graph, state, path, stamp);
        let now = stamps.node_as(node, hashed_as(stamp), Some((recorded, record.start)));
        // what is missing here, which no edge produces, is a dependency, as a missing input is an error
        if !now.exists() && stamp.exists() {
            return Some(Reason::DependencyMissing(path.clone()));
        }
        if !now.same(*stamp) {
            return Some(Reason::InputChanged(path.clone()));
        }
    }
    for (path, stamp) in &record.discovered {
        let recorded = fetched_as(graph, state, path, stamp);
        if !stamps.path_as(path, hashed_as(stamp), Some((recorded, finished(record)))).same(*stamp) {
            return Some(Reason::DiscoveredChanged(path.clone()));
        }
    }
    for (name, hash) in &record.env {
        if stamps.env(name) != *hash {
            return Some(Reason::EnvChanged(name.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Graphs, graph};
    use std::path::{Path, PathBuf};

    /// A build in a temporary directory, whose edges are run by writing their outputs.
    struct Build {
        dir: PathBuf,
        graph: Graph,
        state: State,
    }
    impl Build {
        fn new(name: &str, graph: Graph, files: &[&str]) -> Self {
            let dir = std::env::temp_dir().join(format!("rig-plan-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for file in files {
                write(&dir.join(file), "source");
            }
            let state = State::open(&dir.join(".rig")).unwrap();
            Self { dir, graph, state }
        }
        fn plan(&mut self, targets: &[&str]) -> Result<Vec<(String, Reason)>, Error> {
            let mut stamps = Stamps::new(&self.graph, &self.dir);
            let plan = plan(&self.graph, &self.state, &mut stamps, targets)?;
            Ok(plan.edges.into_iter().map(|(edge, reason)| (self.output(edge), reason)).collect())
        }
        fn output(&self, edge: EdgeId) -> String {
            self.graph.path(self.graph.edge(edge).outputs()[0]).to_string()
        }
        /// Run the edges which must run, each writing its outputs, and discovering `discovered` if it is the edge.
        fn run(&mut self, targets: &[&str], discovered: &[(&str, &[&str])]) -> Vec<String> {
            let mut stamps = Stamps::new(&self.graph, &self.dir);
            let plan = plan(&self.graph, &self.state, &mut stamps, targets).unwrap();
            let mut ran = Vec::new();
            for (edge, _) in plan.edges {
                let inputs = input_stamps(&self.graph, &self.state, edge, &mut stamps);
                let data = self.graph.edge(edge);
                for &output in data.outputs() {
                    write(&self.dir.join(self.graph.path(output)), &format!("built {}", ran.len()));
                }
                let output = self.output(edge);
                let found = discovered.iter().find(|(edge, _)| *edge == output).map_or(&[][..], |(_, paths)| *paths);
                let found = Discovered { inputs: found.iter().map(ToString::to_string).collect(), env: Vec::new() };
                let record = record(&self.graph, edge, inputs, &found, &mut stamps, SystemTime::now(), Duration::ZERO);
                self.state.record(record).unwrap();
                ran.push(output);
            }
            ran
        }
        fn write(&self, path: &str, contents: &str) {
            write(&self.dir.join(path), contents);
        }
    }
    impl Drop for Build {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn cc() -> Graphs {
        graph().rule("cc", &[("command", "cc $in -o $out")])
    }

    /// A chain of edges, `a.o` from `a.c` with dependencies, `lib.a` from it, and `app` from that, with an alias.
    fn chain(dependencies: &[&str]) -> Graph {
        let inputs: Vec<&str> = ["a.c", "|"].into_iter().chain(dependencies.iter().copied()).collect();
        let graph = cc().edge("cc", &["a.o"], &inputs).edge("cc", &["lib.a"], &["a.o"]);
        graph.edge("cc", &["app"], &["lib.a", "main.c"]).edge("alias", &["all"], &["app"]).finish()
    }

    #[test]
    fn hashes_actions_as_they_were_hashed() {
        // as the hashes are in the records of builds, they are those of the bytes of each action together, as they were
        use crate::state::fnv1a;
        let exec = crate::Exec {
            program: "rustc".into(),
            args: vec!["--crate-name".into(), "a".into()],
            env: vec![("A".into(), "b".into())],
            dir: Some("sub".into()),
            stderr: None,
        };
        let mut graph = Graph::new();
        let a = graph.file("a");
        let kind = EdgeKind::Command(exec);
        let cache = Some("toolchain".into());
        let edge = crate::Edge { kind, outputs: vec![a], cache, pass_env: vec!["CC".into()], ..Default::default() };
        let id = graph.add_edge(edge).unwrap();
        let edge = graph.edge(id);
        let mut hash = fnv1a(&action_bytes(edge));
        hash = fnv1a(&[hash.to_le_bytes().as_slice(), b"\0cache", b"toolchain"].concat());
        let mut bytes = hash.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"\0pass-env");
        bytes.extend_from_slice(&2_usize.to_le_bytes());
        bytes.extend_from_slice(b"CC");
        assert_eq!(action_hash(edge), fnv1a(&bytes));
    }

    #[test]
    fn builds_what_has_not_run() {
        let mut build = Build::new("not-run", chain(&["a.h"]), &["a.c", "a.h", "main.c"]);
        let plan = build.plan(&["all"]).unwrap();
        // each edge after those it depends on
        let not_run = |output: &str| (output.to_string(), Reason::NoRecord);
        assert_eq!(plan, [not_run("a.o"), not_run("lib.a"), not_run("app")]);
        assert_eq!(build.run(&["all"], &[]), ["a.o", "lib.a", "app"]);
        assert_eq!(build.plan(&["all"]).unwrap(), []);
        // without targets, the nodes which are not inputs are built
        assert_eq!(build.plan(&[]).unwrap(), []);
    }

    #[test]
    fn builds_again_what_depends_on_a_change() {
        let mut build = Build::new("change", chain(&["a.h"]), &["a.c", "a.h", "main.c"]);
        build.run(&["all"], &[]);
        build.write("a.h", "changed");
        let plan = build.plan(&["app"]).unwrap();
        assert_eq!(plan[0], ("a.o".into(), Reason::InputChanged("a.h".into())));
        assert_eq!(plan[1], ("lib.a".into(), Reason::InputRebuilt("a.o".into())));
        assert_eq!(plan.len(), 3);
        // only what depends on a change is built again
        build.run(&["all"], &[]);
        build.write("main.c", "changed");
        assert_eq!(build.run(&["all"], &[]), ["app"]);
    }

    #[test]
    fn builds_again_when_a_time_goes_back() {
        let mut build = Build::new("back", chain(&["a.h"]), &["a.c", "a.h", "main.c"]);
        build.run(&["all"], &[]);
        // as when an older version is checked out, of the same size
        let file = std::fs::File::options().write(true).open(build.dir.join("a.c")).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)).unwrap();
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::InputChanged("a.c".into()))]);
    }

    #[test]
    fn builds_again_when_an_output_changes() {
        let mut build = Build::new("outputs", chain(&["a.h"]), &["a.c", "a.h", "main.c"]);
        build.run(&["all"], &[]);
        build.write("lib.a", "tampered with");
        assert_eq!(build.plan(&["lib.a"]).unwrap(), [("lib.a".into(), Reason::OutputModified("lib.a".into()))]);
        std::fs::remove_file(build.dir.join("a.o")).unwrap();
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::OutputMissing("a.o".into()))]);
    }

    #[test]
    fn builds_again_when_a_command_changes() {
        let with_flags = |flags: &str| {
            let graph = graph().variable("flags", flags).rule("cc", &[("command", "cc $flags $in -o $out")]);
            let graph = graph.rule("gen", &[("command", "gen $flags"), ("generator", "1")]);
            graph.edge("cc", &["a.o"], &["a.c"]).edge("gen", &["graph"], &["a.c"]).finish()
        };
        let mut build = Build::new("command", with_flags(""), &["a.c"]);
        build.run(&[], &[]);
        build.graph = with_flags("-O");
        // except a generator, whose command is in the graph it writes
        assert_eq!(build.plan(&[]).unwrap(), [("a.o".into(), Reason::CommandChanged)]);
    }

    #[test]
    fn builds_again_when_its_identity_in_the_cache_changes() {
        // as it identifies what else the action depends on, such as a toolchain
        let cached = |identity: &str| {
            graph().cached(identity).rule("cc", &[("command", "cc $in -o $out")]).edge("cc", &["a.o"], &["a.c"]).finish()
        };
        let mut build = Build::new("identity", cached("one"), &["a.c"]);
        build.run(&[], &[]);
        assert_eq!(build.plan(&[]).unwrap(), []);
        build.graph = cached("two");
        assert_eq!(build.plan(&[]).unwrap(), [("a.o".into(), Reason::CommandChanged)]);
    }

    #[test]
    fn builds_again_when_inputs_are_declared() {
        let mut build = Build::new("declared", chain(&["a.h"]), &["a.c", "a.h", "main.c", "b.c"]);
        build.run(&["all"], &[]);
        // a dependency, which is not part of the command
        build.graph = chain(&["a.h", "b.c"]);
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::InputsChanged)]);
    }

    #[test]
    fn order_only_inputs_are_built_first() {
        let graph = cc().edge("cc", &["gen.h"], &["gen.in"]).edge("cc", &["a.o"], &["a.c", "||", "gen.h"]);
        let mut build = Build::new("order-only", graph.finish(), &["a.c", "gen.in"]);
        assert_eq!(build.run(&["a.o"], &[]), ["gen.h", "a.o"]);
        // but do not make what depends on them run
        build.write("gen.in", "changed");
        assert_eq!(build.run(&["a.o"], &[]), ["gen.h"]);
    }

    #[test]
    fn discovered_inputs() {
        let graph = graph().rule("cc", &[("command", "cc $in -o $out"), ("depfile", "$out.d")]);
        let graph = graph.edge("cc", &["gen.h"], &["gen.in"]).edge("cc", &["a.o"], &["a.c", "||", "gen.h"]);
        let mut build = Build::new("discovered", graph.finish(), &["a.c", "gen.in", "sys.h"]);
        build.run(&["a.o"], &[("a.o", &["gen.h", "sys.h"])]);
        build.write("sys.h", "changed");
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::DiscoveredChanged("sys.h".into()))]);
        build.run(&["a.o"], &[("a.o", &["gen.h", "sys.h"])]);
        // a discovered input which an edge produces is built first, and makes what discovered it run
        build.write("gen.in", "changed");
        assert_eq!(build.plan(&["a.o"]).unwrap()[1], ("a.o".into(), Reason::InputRebuilt("gen.h".into())));
        build.run(&["a.o"], &[("a.o", &["gen.h", "sys.h"])]);
        // and one which is removed makes it run, rather than failing
        std::fs::remove_file(build.dir.join("sys.h")).unwrap();
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::DiscoveredChanged("sys.h".into()))]);
    }

    #[test]
    fn aliases() {
        let graph = cc().edge("alias", &["sources"], &["a.c", "b.c"]).edge("cc", &["out"], &["|", "sources"]);
        let graph = graph.edge("alias", &["force"], &[]).edge("cc", &["forced"], &["|", "force"]);
        let mut build = Build::new("aliases", graph.finish(), &["a.c", "b.c"]);
        build.run(&["out"], &[]);
        // an edge which takes an alias as an input runs when an input of the alias changes
        build.write("b.c", "changed");
        assert_eq!(build.plan(&["out"]).unwrap(), [("out".into(), Reason::InputChanged("b.c".into()))]);
        // an alias without inputs, which is not a file, is always out of date
        build.run(&["forced"], &[]);
        assert_eq!(build.plan(&["forced"]).unwrap(), [("forced".into(), Reason::InputRebuilt("force".into()))]);
    }

    #[test]
    fn hashed_edges_compare_contents() {
        let graph = cc().hashed().edge("cc", &["a.o"], &["a.c"]).finish();
        let mut build = Build::new("hashed", graph, &["a.c"]);
        build.run(&["a.o"], &[]);
        // written again unchanged, with a new time, which only changes its time
        let later = SystemTime::now() + Duration::from_secs(5);
        build.write("a.c", "source");
        std::fs::File::options().write(true).open(build.dir.join("a.c")).unwrap().set_modified(later).unwrap();
        assert_eq!(build.plan(&["a.o"]).unwrap(), []);
        // changed, then changed back, which is as it was recorded
        build.write("a.c", "changed");
        assert_eq!(build.plan(&["a.o"]).unwrap(), [("a.o".into(), Reason::InputChanged("a.c".into()))]);
        build.write("a.c", "source");
        assert_eq!(build.plan(&["a.o"]).unwrap(), []);
        // an output written again unchanged is not taken as modified
        let output = std::fs::read(build.dir.join("a.o")).unwrap();
        std::fs::write(build.dir.join("a.o"), output).unwrap();
        assert_eq!(build.plan(&["a.o"]).unwrap(), []);
    }

    #[test]
    fn reuses_the_hash_of_what_was_fetched() {
        let graph = cc().edge("cc", &["a.o"], &["a.c"]).edge("cc", &["app"], &["a.o"]).finish();
        let mut build = Build::new("fetched", graph, &["a.c"]);
        build.run(&["app"], &[]);
        // as if `a.o` was left on a worker, which `app` read there, and fetched since, which its edge recorded by the
        // hash it was left with, and the time and size it was fetched with
        let left = crate::blake3::hash(b"left");
        let Stamp::File { mtime, size } = Stamp::of(&build.dir.join("a.o")) else { panic!() };
        let mut producer = build.state.get("a.o").unwrap().clone();
        producer.outputs[0].1 = Stamp::Content { mtime, size, hash: left };
        let mut consumer = build.state.get("app").unwrap().clone();
        consumer.inputs[0].1 = Stamp::Remote { hash: left, executable: false };
        build.state.record(producer).unwrap();
        build.state.record(consumer).unwrap();
        // while it has that time and size, its hash is taken as it was recorded, rather than it being read, which would
        // find it differs
        assert_eq!(build.plan(&["app"]).unwrap(), []);
        // once it changes, it is read, which its size shows, as a clock may not have moved since it was written
        build.write("a.o", "changed again");
        let reasons = build.plan(&["a.o"]).unwrap();
        assert_eq!(reasons, [("a.o".into(), Reason::OutputModified("a.o".into()))]);
    }

    #[test]
    fn runs_what_has_lost_a_dependency() {
        // a dependency which is missing, and which no edge produces, is not an error, as an input is
        let mut build = Build::new("dependencies", chain(&["gone.rlib"]), &["a.c", "main.c"]);
        assert_eq!(build.run(&["app"], &[]), ["a.o", "lib.a", "app"]);
        assert_eq!(build.plan(&["app"]).unwrap(), []);
        // one which it had, which is removed, runs it, as its use may have been removed with it
        build.write("gone.rlib", "library");
        assert_eq!(build.run(&["app"], &[]), ["a.o", "lib.a", "app"]);
        std::fs::remove_file(build.dir.join("gone.rlib")).unwrap();
        let reasons = build.plan(&["a.o"]).unwrap();
        assert_eq!(reasons, [("a.o".into(), Reason::DependencyMissing("gone.rlib".into()))]);
        // and once it has run without it, it is up to date
        assert_eq!(build.run(&["app"], &[]), ["a.o", "lib.a", "app"]);
        assert_eq!(build.plan(&["app"]).unwrap(), []);
    }

    #[test]
    fn reports_missing_inputs_and_cycles() {
        let graph = cc().edge("cc", &["a.o"], &["a.c", "a.h"]).edge("cc", &["app"], &["a.o", "||", "after"]);
        let mut build = Build::new("errors", graph.finish(), &["a.c"]);
        let missing = "\"a.h\", needed by \"a.o\", is missing and no edge produces it";
        assert_eq!(build.plan(&["app"]).unwrap_err().to_string(), missing);
        // as is what an edge is built after
        build.write("a.h", "header");
        let missing = "\"after\", needed by \"app\", is missing and no edge produces it";
        assert_eq!(build.plan(&["app"]).unwrap_err().to_string(), missing);
        assert_eq!(build.plan(&["nothing"]).unwrap_err().to_string(), "unknown target \"nothing\"");
        let graph = cc().edge("cc", &["a"], &["b"]).edge("cc", &["b"], &["c"]).edge("cc", &["c"], &["a"]);
        let mut build = Build::new("cycle", graph.finish(), &[]);
        assert_eq!(build.plan(&["a"]).unwrap_err().to_string(), "dependency cycle: a -> b -> c -> a");
    }
}
