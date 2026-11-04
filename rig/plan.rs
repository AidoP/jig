//! Planning a build: which edges must run to bring targets up to date, and why.
//!
//! An edge must run if it has no record, its outputs or command have changed, one of its outputs is missing or is not
//! what the edge produced, the inputs it declares have changed, an input is produced by an edge which must run, or the
//! stamp of an input, declared or discovered from its depfile, differs from the one recorded. Order-only inputs are
//! built first, but do not make an edge run. The command of a generator edge may change without it running, as ninja
//! allows.
//!
//! An alias is out of date if an input of it is produced by an edge which must run, or if it has no inputs and is not a
//! file. An edge which takes an alias as an input takes the inputs of the alias instead, so that it runs when one of
//! them changes.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::Error;
use crate::depfile::Discovered;
use crate::graph::{Action, EdgeId, EdgeKind, EdgeRef, Graph, NodeId};
use crate::stamp::{Stamp, Stamps};
use crate::state::{Record, State, fnv1a};

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

/// Returns the hash of an edge's action, which it runs again when it changes.
#[must_use]
pub fn action_hash(edge: EdgeRef) -> u64 {
    match edge.kind() {
        EdgeKind::Alias => 0,
        EdgeKind::Command(Action::Shell(command)) => fnv1a(command.as_bytes()),
        // each part with its length before it, so that parts which run together hash differently
        EdgeKind::Command(Action::Exec(exec)) => {
            let mut bytes = Vec::new();
            let mut part = |part: &str| {
                bytes.extend_from_slice(&part.len().to_le_bytes());
                bytes.extend_from_slice(part.as_bytes());
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
            fnv1a(&bytes)
        },
    }
}

/// Returns the inputs an edge declares, explicit and implicit, with the inputs of each alias in place of the alias.
#[must_use]
pub fn declared_inputs(graph: &Graph, edge: EdgeRef) -> Vec<NodeId> {
    let mut inputs = Vec::new();
    let mut seen = HashSet::new();
    let mut stack: Vec<NodeId> = edge.inputs().iter().chain(edge.implicit()).rev().copied().collect();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        let alias = graph.producer(node).map(|edge| graph.edge(edge)).filter(|edge| *edge.kind() == EdgeKind::Alias);
        match alias {
            Some(alias) => stack.extend(alias.inputs().iter().chain(alias.implicit()).rev()),
            None => inputs.push(node),
        }
    }
    inputs
}

/// Returns the stamps of the inputs an edge declares, as they are when it starts, to record once it has run.
pub fn input_stamps(graph: &Graph, edge: EdgeId, stamps: &mut Stamps) -> Vec<(String, Stamp)> {
    let inputs = declared_inputs(graph, graph.edge(edge));
    inputs.into_iter().map(|node| (graph.path(node).to_string(), stamps.node(node))).collect()
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
    for &output in edge.outputs().iter().chain(edge.implicit_outputs()) {
        stamps.forget(output);
        outputs.push((graph.path(output).to_string(), stamps.node(output)));
    }
    let env = discovered.env.iter().map(|name| (name.clone(), stamps.env(name))).collect();
    let discovered = discovered.inputs.iter().map(|path| (path.clone(), stamps.path(path))).collect();
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
/// produced by an edge and is not an input of one, as ninja builds.
#[must_use]
pub fn default_targets(graph: &Graph) -> Vec<NodeId> {
    if !graph.defaults().is_empty() {
        return graph.defaults().to_vec();
    }
    let mut used = vec![false; graph.nodes()];
    for edge in graph.edges() {
        for node in edge.inputs().iter().chain(edge.implicit()).chain(edge.order_only()) {
            used[node.0 as usize] = true;
        }
    }
    let produced = graph.edges().flat_map(|edge| edge.outputs().iter().chain(edge.implicit_outputs()));
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
    let targets = if targets.is_empty() {
        default_targets(graph)
    } else {
        let nodes = targets.iter().map(|&path| graph.node(path).ok_or_else(|| Error::UnknownTarget(path.into())));
        nodes.collect::<Result<_, _>>()?
    };
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
        let lists = [data.outputs(), data.implicit_outputs(), data.inputs(), data.implicit(), data.order_only()];
        stack.extend(lists.into_iter().flatten());
        for (path, _) in state.get(&first_output(graph, edge)).into_iter().flat_map(|record| &record.discovered) {
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
        // each entry is an edge, its inputs, of which the first are those it declares, and the next to visit
        let mut stack = vec![self.frame(start)];
        self.status[start.0 as usize] = Status::Visiting;
        while let Some((edge, inputs, declared, next)) = stack.last_mut() {
            let (edge, declared) = (*edge, *declared);
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
                // a discovered input which is missing makes its edge run, rather than failing
                if index < declared && !self.stamps.node(input).exists() {
                    let needed_by = data.outputs().iter().chain(data.implicit_outputs()).next();
                    let needed_by = needed_by.map(|&node| self.graph.path(node).to_string());
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
    /// Returns an edge to visit, with its inputs: those it declares, explicit, implicit and order-only, then those it
    /// discovered when it last ran which an edge produces, which are built first, as ninja does.
    fn frame(&self, edge: EdgeId) -> (EdgeId, Vec<NodeId>, usize, usize) {
        let data = self.graph.edge(edge);
        let inputs = data.inputs().iter().chain(data.implicit()).chain(data.order_only());
        let mut inputs: Vec<NodeId> = inputs.copied().collect();
        let declared = inputs.len();
        inputs.extend(self.discovered(edge));
        (edge, inputs, declared, 0)
    }
    fn discovered(&self, edge: EdgeId) -> Vec<NodeId> {
        discovered_nodes(self.graph, self.state, edge)
    }
    fn first_output(&self, edge: EdgeId) -> String {
        first_output(self.graph, edge)
    }
    /// Returns why an edge must run, once the edges which produce its inputs have been visited, or `None` if it is up
    /// to date.
    fn evaluate(&mut self, id: EdgeId) -> Option<Reason> {
        let status = &self.status;
        reason(self.graph, self.state, self.stamps, id, &|producer| status[producer.0 as usize] == Status::Dirty)
    }
}

/// Returns the first output of an edge, which its record is kept by.
pub(crate) fn first_output(graph: &Graph, edge: EdgeId) -> String {
    let edge = graph.edge(edge);
    let output = edge.outputs().iter().chain(edge.implicit_outputs()).next();
    output.map(|&node| graph.path(node).to_string()).unwrap_or_default()
}

/// Returns the nodes an edge discovered when it last ran which an edge produces, which are built before it.
pub(crate) fn discovered_nodes(graph: &Graph, state: &State, edge: EdgeId) -> Vec<NodeId> {
    let record = state.get(&first_output(graph, edge));
    let paths = record.into_iter().flat_map(|record| &record.discovered);
    let nodes = paths.filter_map(|(path, _)| graph.node(path));
    nodes.filter(|&node| graph.producer(node).is_some()).collect()
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
    let rebuilt = edge.inputs().iter().chain(edge.implicit()).chain(&discovered).find(|&&input| {
        graph.producer(input).is_some_and(rebuilt)
    });
    if *edge.kind() == EdgeKind::Alias {
        if let Some(&input) = rebuilt {
            return Some(Reason::InputRebuilt(graph.path(input).into()));
        }
        let forced = edge.inputs().is_empty() && edge.implicit().is_empty();
        let missing = edge.outputs().iter().any(|&output| !stamps.node(output).exists());
        return (forced && missing).then_some(Reason::Forced);
    }
    let outputs: Vec<NodeId> = edge.outputs().iter().chain(edge.implicit_outputs()).copied().collect();
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
    for (&output, (path, stamp)) in outputs.iter().zip(&record.outputs) {
        let now = stamps.node(output);
        if !now.exists() {
            return Some(Reason::OutputMissing(path.clone()));
        }
        if now != *stamp {
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
        if stamps.node(node) != *stamp {
            return Some(Reason::InputChanged(path.clone()));
        }
    }
    for (path, stamp) in &record.discovered {
        if stamps.path(path) != *stamp {
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
                let inputs = input_stamps(&self.graph, edge, &mut stamps);
                let data = self.graph.edge(edge);
                for &output in data.outputs().iter().chain(data.implicit_outputs()) {
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

    /// A chain of edges, `a.o` from `a.c` with implicit inputs, `lib.a` from it, and `app` from that, with an alias.
    fn chain(implicit: &[&str]) -> Graph {
        let inputs: Vec<&str> = ["a.c", "|"].into_iter().chain(implicit.iter().copied()).collect();
        let graph = cc().edge("cc", &["a.o"], &inputs).edge("cc", &["lib.a"], &["a.o"]);
        graph.edge("cc", &["app"], &["lib.a", "main.c"]).edge("phony", &["all"], &["app"]).finish()
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
            graph.edge("cc", &["a.o"], &["a.c"]).edge("gen", &["build.ninja"], &["a.c"]).finish()
        };
        let mut build = Build::new("command", with_flags(""), &["a.c"]);
        build.run(&[], &[]);
        build.graph = with_flags("-O");
        // except a generator, as in ninja
        assert_eq!(build.plan(&[]).unwrap(), [("a.o".into(), Reason::CommandChanged)]);
    }

    #[test]
    fn builds_again_when_inputs_are_declared() {
        let mut build = Build::new("declared", chain(&["a.h"]), &["a.c", "a.h", "main.c", "b.c"]);
        build.run(&["all"], &[]);
        // an implicit input, which is not part of the command
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
        let graph = cc().edge("phony", &["sources"], &["a.c", "b.c"]).edge("cc", &["out"], &["|", "sources"]);
        let graph = graph.edge("phony", &["force"], &[]).edge("cc", &["forced"], &["|", "force"]);
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
    fn reports_missing_inputs_and_cycles() {
        let mut build = Build::new("errors", chain(&["a.h"]), &["a.c", "main.c"]);
        let missing = "\"a.h\", needed by \"a.o\", is missing and no edge produces it";
        assert_eq!(build.plan(&["app"]).unwrap_err().to_string(), missing);
        assert_eq!(build.plan(&["nothing"]).unwrap_err().to_string(), "unknown target \"nothing\"");
        let graph = cc().edge("cc", &["a"], &["b"]).edge("cc", &["b"], &["c"]).edge("cc", &["c"], &["a"]);
        let mut build = Build::new("cycle", graph.finish(), &[]);
        assert_eq!(build.plan(&["a"]).unwrap_err().to_string(), "dependency cycle: a -> b -> c -> a");
    }
}
