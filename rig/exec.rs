//! Running a build: the edges a [`plan`](crate::plan::plan) finds must run, in parallel, each once those it depends on
//! have run.
//!
//! Each edge runs its command with `sh -c` in the build's directory, on a thread of its own, at most
//! [`Options::jobs`] at once. Its output, stdout and stderr together, is captured and given to an [`Observer`] once it
//! finishes, while an edge in the console pool runs alone among console edges with the terminal, and the output of
//! the others is held until it finishes. An edge which succeeds is recorded as soon as it does, with the inputs its
//! depfile lists, so that an interrupted build loses only the edges which were running.
//!
//! The outputs of a restat edge are found again once it has run, and if none of them changed, an edge which would run
//! only because it depends on them is checked again, and does not run if it is otherwise up to date.
//!
//! A generator edge is run like any other. A build whose graph is generated should first build the file it is read
//! from, then read the graph again if that ran, as ninja does.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::graph::{Action, EdgeId, EdgeKind, Graph, NodeId, Pool};
use crate::plan::{self, Reason};
use crate::stamp::{Stamp, Stamps};
use crate::state::State;
use crate::jobserver::Jobserver;
use crate::{Error, depfile, sys};

/// How a build runs.
#[derive(Clone, Debug)]
pub struct Options {
    /// The directory the paths of the graph are relative to, where the commands run.
    pub dir: PathBuf,
    /// The most edges which run at once.
    pub jobs: usize,
    /// The number of edges which may fail before no more are started, or `0` for no limit, as ninja's `-k`.
    pub keep_going: usize,
    /// Whether the commands share the jobs through a jobserver, which rig joins if it is run by one.
    pub jobserver: bool,
    /// Set to stop the build, as an interrupt does: no more edges start, and those running are sent `SIGINT`.
    pub cancel: Arc<AtomicBool>,
    /// Environment variables every command is run with, beside those rig is run with.
    pub env: Vec<(String, String)>,
}
impl Options {
    /// Options for a build in a directory, with as many jobs as the machine runs at once, which stops at the first
    /// failure.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let jobs = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self { dir: dir.into(), jobs, keep_going: 1, jobserver: true, cancel: Arc::default(), env: Vec::new() }
    }
}

/// How an edge which ran finished.
#[derive(Debug)]
pub enum Outcome {
    Succeeded,
    /// The command exited unsuccessfully.
    Failed(ExitStatus),
    /// The command could not be run, or its depfile could not be read.
    Error(String),
    /// The command did not succeed, once the build was interrupted, which is not counted as a failure.
    Interrupted,
}
impl Outcome {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// How far a build has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The number of edges which may run.
    pub total: usize,
    pub started: usize,
    pub finished: usize,
    pub running: usize,
}

/// What is told of a build as it runs.
pub trait Observer {
    /// An edge has started, for the reason the plan gave.
    fn started(&mut self, graph: &Graph, edge: EdgeId, reason: &Reason, progress: Progress) {
        let _ = (graph, edge, reason, progress);
    }
    /// An edge has finished, with its output, stdout and stderr together, which is empty for a console edge.
    fn finished(&mut self, graph: &Graph, edge: EdgeId, outcome: &Outcome, output: &[u8], progress: Progress) {
        let _ = (graph, edge, outcome, output, progress);
    }
}

/// What a build did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The edges which ran, including those which failed.
    pub ran: usize,
    pub failed: usize,
    /// The edges which did not run, as an edge they depend on failed, or the build stopped.
    pub not_run: usize,
    /// The edges which did not run, as the restat edges they depend on did not change their outputs.
    pub restat_skipped: usize,
    /// Whether the build was interrupted, by a signal or [`Options::cancel`].
    pub interrupted: bool,
}
impl Report {
    /// Whether every edge which had to run succeeded.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.not_run == 0 && !self.interrupted
    }
}

/// How long the processes of an interrupted build have to stop before they are killed.
const KILL_AFTER: Duration = Duration::from_secs(1);

/// What a thread which runs an edge sends back.
enum Message {
    /// The command of an edge has started, as the process with an id, which leads its process group.
    Spawned(usize, u32),
    Finished(Finished),
}

/// An edge which has finished.
struct Finished {
    /// The index of the edge in the plan.
    index: usize,
    outcome: Outcome,
    output: Vec<u8>,
    start: SystemTime,
    duration: std::time::Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Waiting for the edges it depends on.
    Waiting,
    Running,
    /// Ran, or was skipped, with whether its outputs changed.
    Done { changed: bool },
    Failed,
}

/// Build targets, given by their paths, or the default targets if none are given.
///
/// # Errors
///
/// If the build cannot be planned, or the state cannot be written.
pub fn build(
    graph: &Graph,
    state: &mut State,
    options: &Options,
    targets: &[&str],
    observer: &mut dyn Observer,
) -> Result<Report, Error> {
    let mut stamps = Stamps::new(graph, &options.dir).with_env(&options.env);
    let plan = plan::plan(graph, state, &mut stamps, targets)?;
    let edges: Vec<EdgeId> = plan.edges.iter().map(|&(edge, _)| edge).collect();
    let index: HashMap<EdgeId, usize> = edges.iter().enumerate().map(|(index, &edge)| (edge, index)).collect();
    let deps: Vec<Vec<usize>> = edges.iter().map(|&edge| planned_deps(graph, state, edge, &index)).collect();
    let mut dependents = vec![Vec::new(); edges.len()];
    for (index, deps) in deps.iter().enumerate() {
        for &dep in deps {
            dependents[dep].push(index);
        }
    }
    let mut run = Run {
        graph,
        state,
        options,
        stamps,
        edges,
        reasons: plan.edges.into_iter().map(|(_, reason)| reason).collect(),
        index,
        waiting: deps.iter().map(Vec::len).collect(),
        deps,
        dependents,
        status: Vec::new(),
        ready: BTreeSet::new(),
        inputs: HashMap::new(),
        before: HashMap::new(),
        progress: Progress::default(),
        report: Report::default(),
        console: false,
        held: Vec::new(),
        error: None,
        jobserver: None,
        tokens: HashMap::new(),
        processes: HashMap::new(),
    };
    if options.jobserver && !run.edges.is_empty() {
        run.jobserver = Jobserver::from_env().or_else(|| Jobserver::create(options.jobs.max(1)).ok());
    }
    run.status = vec![Status::Waiting; run.edges.len()];
    run.progress.total = run.edges.len();
    run.ready = (0..run.edges.len()).filter(|&index| run.waiting[index] == 0).collect();
    run.run(observer)?;
    run.report.not_run = run.status.iter().filter(|status| matches!(status, Status::Waiting)).count();
    Ok(run.report)
}

/// Returns the edges in the plan which an edge depends on, through aliases, by its inputs, declared and discovered.
fn planned_deps(graph: &Graph, state: &State, edge: EdgeId, index: &HashMap<EdgeId, usize>) -> Vec<usize> {
    let data = graph.edge(edge);
    let inputs = data.inputs().iter().chain(data.implicit()).chain(data.order_only());
    let mut nodes: Vec<NodeId> = inputs.copied().collect();
    nodes.extend(plan::discovered_nodes(graph, state, edge));
    let mut deps = Vec::new();
    let mut seen = HashSet::new();
    while let Some(node) = nodes.pop() {
        let Some(producer) = graph.producer(node) else {
            continue;
        };
        if !seen.insert(producer) {
            continue;
        }
        if let Some(&dep) = index.get(&producer) {
            deps.push(dep);
        } else if *graph.edge(producer).kind() == EdgeKind::Alias {
            let alias = graph.edge(producer);
            nodes.extend(alias.inputs().iter().chain(alias.implicit()).chain(alias.order_only()));
        }
    }
    deps.sort_unstable();
    deps
}

struct Run<'g, 's> {
    graph: &'g Graph,
    state: &'s mut State,
    options: &'s Options,
    stamps: Stamps<'g>,
    /// The edges of the plan, in its order, which each run after those they depend on.
    edges: Vec<EdgeId>,
    reasons: Vec<Reason>,
    index: HashMap<EdgeId, usize>,
    /// The edges of the plan which each depends on, and depend on it.
    deps: Vec<Vec<usize>>,
    dependents: Vec<Vec<usize>>,
    /// The number of edges each is waiting for.
    waiting: Vec<usize>,
    status: Vec<Status>,
    /// The edges which are ready to run, by their order in the plan.
    ready: BTreeSet<usize>,
    /// The stamps of the declared inputs of each running edge, when it started.
    inputs: HashMap<usize, Vec<(String, Stamp)>>,
    /// The stamps of the outputs of each running restat edge, before it started.
    before: HashMap<usize, Vec<Stamp>>,
    progress: Progress,
    report: Report,
    /// Whether a console edge is running.
    console: bool,
    /// The edges which finished while a console edge was running, whose output is held until it finishes.
    held: Vec<(usize, Outcome, Vec<u8>)>,
    /// The first error writing the state, which stops the build.
    error: Option<Error>,
    jobserver: Option<Jobserver>,
    /// The token each running edge holds, which each but one does.
    tokens: HashMap<usize, u8>,
    /// The process group of each running edge which is not in the console pool.
    processes: HashMap<usize, u32>,
}

impl Run<'_, '_> {
    fn run(&mut self, observer: &mut dyn Observer) -> Result<(), Error> {
        let (sender, receiver) = std::sync::mpsc::channel::<Message>();
        let interrupts = sys::Interrupts::catch();
        let makeflags = self.jobserver.as_ref().and_then(Jobserver::makeflags).map(String::from);
        // when the build was interrupted, until its processes are killed
        let mut interrupted_at = None;
        std::thread::scope(|scope| {
            loop {
                let cancelled = self.options.cancel.load(Ordering::SeqCst).then_some(sys::SIGINT);
                let interrupted = interrupts.interrupted().or(cancelled);
                if let Some(signal) = interrupted && !self.report.interrupted {
                    self.report.interrupted = true;
                    interrupted_at = Some(Instant::now());
                    for &group in self.processes.values() {
                        sys::signal_group(group, signal);
                    }
                }
                // a process which ignores the signal, as a shell's background job ignores `SIGINT`, is killed
                if let Some(at) = interrupted_at && at.elapsed() >= KILL_AFTER {
                    interrupted_at = None;
                    for &group in self.processes.values() {
                        sys::signal_group(group, sys::SIGKILL);
                    }
                }
                // whether an edge is ready but waits for a token
                let mut waiting_for_token = false;
                while self.progress.running < self.options.jobs.max(1) && !self.stopping() {
                    let Some(index) = self.next_ready() else {
                        break;
                    };
                    if self.restat_clean(index) {
                        self.report.restat_skipped += 1;
                        self.progress.total -= 1;
                        self.done(index, false);
                        continue;
                    }
                    // each edge but the first running takes a token
                    if self.progress.running > 0 && let Some(jobserver) = &self.jobserver {
                        let Some(token) = jobserver.try_acquire() else {
                            self.ready.insert(index);
                            waiting_for_token = true;
                            break;
                        };
                        self.tokens.insert(index, token);
                    }
                    self.start(index, observer);
                    let console = self.graph.edge(self.edges[index]).pool() == Pool::Console;
                    let Action::Shell(command) = self.action(index).clone();
                    let (dir, sender, makeflags) = (self.options.dir.clone(), sender.clone(), makeflags.clone());
                    let env = self.options.env.clone();
                    scope.spawn(move || {
                        let start = SystemTime::now();
                        let clock = Instant::now();
                        let spawned = |pid| {
                            let _ = sender.send(Message::Spawned(index, pid));
                        };
                        let environment = Environment { makeflags: makeflags.as_deref(), env: &env };
                        let (outcome, output) = run_command(&command, &dir, console, &environment, &spawned);
                        let finished = Finished { index, outcome, output, start, duration: clock.elapsed() };
                        let _ = sender.send(Message::Finished(finished));
                    });
                }
                if self.progress.running == 0 {
                    break;
                }
                // tokens are given back by the commands as well as the edges, so they are looked for again soon,
                // while an interrupt is looked for less often
                let timeout = Duration::from_millis(if waiting_for_token { 5 } else { 50 });
                match receiver.recv_timeout(timeout) {
                    Ok(Message::Spawned(index, pid)) => {
                        if self.graph.edge(self.edges[index]).pool() != Pool::Console {
                            self.processes.insert(index, pid);
                            if self.report.interrupted {
                                sys::signal_group(pid, sys::SIGINT);
                            }
                        }
                    },
                    Ok(Message::Finished(finished)) => {
                        self.processes.remove(&finished.index);
                        if let Some(token) = self.tokens.remove(&finished.index)
                            && let Some(jobserver) = &self.jobserver
                        {
                            jobserver.release(token);
                        }
                        self.finish(finished, observer);
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    /// Whether no more edges are started, as too many have failed or the state could not be written.
    fn stopping(&self) -> bool {
        let failed = self.options.keep_going != 0 && self.report.failed >= self.options.keep_going;
        failed || self.error.is_some() || self.report.interrupted
    }
    /// Returns the first edge which is ready to run, other than one in the console pool while another runs.
    fn next_ready(&mut self) -> Option<usize> {
        let index = *self.ready.iter().find(|&&index| {
            !self.console || self.graph.edge(self.edges[index]).pool() != Pool::Console
        })?;
        self.ready.remove(&index);
        Some(index)
    }
    fn action(&self, index: usize) -> &Action {
        match self.graph.edge(self.edges[index]).kind() {
            EdgeKind::Command(action) => action,
            // the plan has no aliases
            EdgeKind::Alias => unreachable!("an alias is not run"),
        }
    }
    /// Whether an edge which would run only as an input is built again is up to date after all, as a restat edge it
    /// depends on left its outputs alone.
    ///
    /// It is checked again once the edges it depends on have run, if one of them left its outputs alone, by the inputs
    /// which make it run, so that another which it depends on only through an order-only input may have changed.
    fn restat_clean(&mut self, index: usize) -> bool {
        let unchanged = |dep: &usize| self.status[*dep] == Status::Done { changed: false };
        if !self.deps[index].iter().any(unchanged) || !matches!(self.reasons[index], Reason::InputRebuilt(_)) {
            return false;
        }
        let (graph, index_of, status) = (self.graph, &self.index, &self.status);
        let changed = |edge: EdgeId| changed(graph, edge, index_of, status, &mut HashSet::new());
        plan::reason(graph, self.state, &mut self.stamps, self.edges[index], &changed).is_none()
    }
    fn start(&mut self, index: usize, observer: &mut dyn Observer) {
        let edge = self.edges[index];
        self.inputs.insert(index, plan::input_stamps(self.graph, edge, &mut self.stamps));
        let data = self.graph.edge(edge);
        // the directories of the outputs and depfile are created first, as ninja creates them, which the command
        // fails without if it needs them
        let outputs = data.outputs().iter().chain(data.implicit_outputs()).map(|&node| self.graph.path(node));
        for path in outputs.chain(data.depfile()) {
            if let Some(dir) = Path::new(path).parent().filter(|dir| !dir.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(self.options.dir.join(dir));
            }
        }
        if data.restat() {
            let outputs = data.outputs().iter().chain(data.implicit_outputs());
            let before = outputs.map(|&output| self.stamps.node(output)).collect();
            self.before.insert(index, before);
        }
        if data.pool() == Pool::Console {
            self.console = true;
        }
        self.status[index] = Status::Running;
        self.progress.started += 1;
        self.progress.running += 1;
        observer.started(self.graph, edge, &self.reasons[index], self.progress);
    }
    fn finish(&mut self, finished: Finished, observer: &mut dyn Observer) {
        let Finished { index, mut outcome, output, start, duration } = finished;
        let edge = self.edges[index];
        let data = self.graph.edge(edge);
        self.progress.running -= 1;
        self.progress.finished += 1;
        self.report.ran += 1;
        let inputs = self.inputs.remove(&index).unwrap_or_default();
        if outcome.succeeded() {
            let discovered = match data.depfile() {
                Some(path) => read_depfile(&self.options.dir.join(path)),
                None => Ok(depfile::Discovered::default()),
            };
            match discovered {
                Ok(discovered) => {
                    let record = plan::record(self.graph, edge, inputs, &discovered, &mut self.stamps, start, duration);
                    let changed = match self.before.remove(&index) {
                        Some(before) => record.outputs.iter().map(|(_, stamp)| *stamp).ne(before),
                        None => true,
                    };
                    if let Err(error) = self.state.record(record) {
                        self.error.get_or_insert(error);
                    }
                    self.done(index, changed);
                },
                Err(error) => outcome = Outcome::Error(error),
            }
        }
        if !outcome.succeeded() && self.report.interrupted {
            outcome = Outcome::Interrupted;
        }
        if !outcome.succeeded() {
            self.status[index] = Status::Failed;
            if !matches!(outcome, Outcome::Interrupted) {
                self.report.failed += 1;
            }
        }
        if data.pool() == Pool::Console {
            self.console = false;
            observer.finished(self.graph, edge, &outcome, &output, self.progress);
            for (index, outcome, output) in std::mem::take(&mut self.held) {
                observer.finished(self.graph, self.edges[index], &outcome, &output, self.progress);
            }
        } else if self.console {
            self.held.push((index, outcome, output));
        } else {
            observer.finished(self.graph, edge, &outcome, &output, self.progress);
        }
    }
    /// Mark an edge as done, making ready the edges which were waiting only for it.
    fn done(&mut self, index: usize, changed: bool) {
        self.status[index] = Status::Done { changed };
        for &dependent in &self.dependents[index] {
            self.waiting[dependent] -= 1;
            if self.waiting[dependent] == 0 {
                self.ready.insert(dependent);
            }
        }
    }
}

/// Whether an edge changed its outputs in this build: an edge of the plan which ran and changed them, or an alias of
/// such an edge.
fn changed(
    graph: &Graph,
    edge: EdgeId,
    index: &HashMap<EdgeId, usize>,
    status: &[Status],
    seen: &mut HashSet<EdgeId>,
) -> bool {
    if let Some(&index) = index.get(&edge) {
        return status[index] == Status::Done { changed: true };
    }
    let data = graph.edge(edge);
    if *data.kind() != EdgeKind::Alias || !seen.insert(edge) {
        return false;
    }
    let inputs = data.inputs().iter().chain(data.implicit());
    let producers: Vec<EdgeId> = inputs.filter_map(|&node| graph.producer(node)).collect();
    producers.into_iter().any(|producer| changed(graph, producer, index, status, seen))
}

/// Returns what a depfile lists, or nothing if it is missing, as a command need not write one.
fn read_depfile(path: &Path) -> Result<depfile::Discovered, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            depfile::parse(&String::from_utf8_lossy(&bytes)).map_err(|error| format!("depfile {path:?}: {error}"))
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(depfile::Discovered::default()),
        Err(error) => Err(format!("depfile {path:?}: {error}")),
    }
}

/// What a command is run with, beside the environment rig is run with.
struct Environment<'a> {
    /// The value of `MAKEFLAGS` which gives it the jobserver, if rig created one.
    makeflags: Option<&'a str>,
    env: &'a [(String, String)],
}

/// Run a command with `sh -c` in a directory, returning how it finished and its output, stdout and stderr together,
/// with the jobserver's `MAKEFLAGS` if there is one, telling `spawned` the id of its process once it has started.
///
/// A console command has the terminal, and no output is captured, and is in rig's process group, so that it is sent
/// what the terminal sends, such as `SIGINT`. Any other has no stdin, and leads a process group of its own, which the
/// build signals when it is interrupted.
fn run_command(
    command: &str,
    dir: &Path,
    console: bool,
    environment: &Environment,
    spawned: &dyn Fn(u32),
) -> (Outcome, Vec<u8>) {
    use std::os::unix::process::CommandExt;
    let mut sh = Command::new("/bin/sh");
    sh.arg("-c").arg(command).current_dir(dir);
    sh.envs(environment.env.iter().map(|(name, value)| (name, value)));
    if let Some(makeflags) = environment.makeflags {
        sh.env("MAKEFLAGS", makeflags).env("CARGO_MAKEFLAGS", makeflags);
    }
    if console {
        let child = sh.spawn().and_then(|mut child| {
            spawned(child.id());
            child.wait()
        });
        return match child {
            Ok(status) if status.success() => (Outcome::Succeeded, Vec::new()),
            Ok(status) => (Outcome::Failed(status), Vec::new()),
            Err(error) => (Outcome::Error(format!("failed to run `sh`: {error}")), Vec::new()),
        };
    }
    let pipe = std::io::pipe().and_then(|(reader, writer)| Ok((reader, writer.try_clone()?, writer)));
    let (mut reader, stdout, stderr) = match pipe {
        Ok(pipe) => pipe,
        Err(error) => return (Outcome::Error(format!("failed to create a pipe: {error}")), Vec::new()),
    };
    sh.stdin(Stdio::null()).stdout(stdout).stderr(stderr).process_group(0);
    let child = sh.spawn();
    // the command holds the pipe's writers, which must be closed for the output to end
    drop(sh);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return (Outcome::Error(format!("failed to run `sh`: {error}")), Vec::new()),
    };
    spawned(child.id());
    let mut output = Vec::new();
    let read = reader.read_to_end(&mut output);
    let outcome = match (child.wait(), read) {
        (Ok(status), Ok(_)) if status.success() => Outcome::Succeeded,
        (Ok(status), Ok(_)) => Outcome::Failed(status),
        (Err(error), _) | (_, Err(error)) => Outcome::Error(format!("failed to wait for `sh`: {error}")),
    };
    (outcome, output)
}

/// Shows a build as ninja does on a terminal which is not smart: the status and description of each edge once it
/// finishes, or as it starts for a console edge, which then has the terminal, followed by its output. A failed edge
/// is shown with its outputs and command. With `quiet`, only the output of the commands and failures are shown, and
/// with `verbose`, each edge is shown by its command rather than its description.
pub struct Console<W: std::io::Write> {
    pub out: W,
    pub quiet: bool,
    pub verbose: bool,
}
impl<W: std::io::Write> Console<W> {
    fn status(&mut self, graph: &Graph, edge: EdgeId, progress: Progress) {
        if self.quiet {
            return;
        }
        let data = graph.edge(edge);
        let description = match (data.description(), data.kind()) {
            (description, EdgeKind::Command(Action::Shell(command))) if description.is_empty() || self.verbose => {
                command.as_str()
            },
            (description, _) => description,
        };
        let _ = writeln!(self.out, "[{}/{}] {description}", progress.finished, progress.total);
    }
}
impl<W: std::io::Write> Observer for Console<W> {
    fn started(&mut self, graph: &Graph, edge: EdgeId, _: &Reason, progress: Progress) {
        if graph.edge(edge).pool() == Pool::Console {
            self.status(graph, edge, progress);
            let _ = self.out.flush();
        }
    }
    fn finished(&mut self, graph: &Graph, edge: EdgeId, outcome: &Outcome, output: &[u8], progress: Progress) {
        let data = graph.edge(edge);
        if data.pool() != Pool::Console {
            self.status(graph, edge, progress);
        }
        if !outcome.succeeded() && !matches!(outcome, Outcome::Interrupted) {
            let outputs = data.outputs().iter().chain(data.implicit_outputs());
            let outputs: Vec<&str> = outputs.map(|&node| graph.path(node)).collect();
            let _ = writeln!(self.out, "FAILED: {}", outputs.join(" "));
            if let EdgeKind::Command(Action::Shell(command)) = data.kind() {
                let _ = writeln!(self.out, "{command}");
            }
            if let Outcome::Error(error) = outcome {
                let _ = writeln!(self.out, "{error}");
            }
        }
        let _ = self.out.write_all(output);
        if !output.is_empty() && !output.ends_with(b"\n") {
            let _ = writeln!(self.out);
        }
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Graphs, graph};
    use std::time::Duration;

    /// A build in a temporary directory, which runs real commands.
    struct Build {
        dir: PathBuf,
        graph: Graph,
        state: State,
        options: Options,
    }
    impl Build {
        fn new(name: &str, graph: Graph, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("rig-exec-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (path, contents) in files {
                std::fs::write(dir.join(path), contents).unwrap();
            }
            let state = State::open(&dir.join(".rig")).unwrap();
            let options = Options::new(&dir);
            Self { dir, graph, state, options }
        }
        /// Build targets, returning the report and the first output of each edge which finished, with its output.
        fn build(&mut self, targets: &[&str]) -> (Report, Vec<(String, String)>) {
            let mut observer = Recorder::default();
            let report = build(&self.graph, &mut self.state, &self.options, targets, &mut observer).unwrap();
            (report, observer.finished)
        }
        /// Returns the first outputs of the edges which ran.
        fn ran(&mut self, targets: &[&str]) -> Vec<String> {
            let (report, finished) = self.build(targets);
            assert!(report.succeeded(), "{report:?} {finished:?}");
            finished.into_iter().map(|(output, _)| output).collect()
        }
        fn read(&self, path: &str) -> String {
            std::fs::read_to_string(self.dir.join(path)).unwrap()
        }
        fn write(&self, path: &str, contents: &str) {
            std::fs::write(self.dir.join(path), contents).unwrap();
        }
    }
    impl Drop for Build {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[derive(Default)]
    struct Recorder {
        finished: Vec<(String, String)>,
    }
    impl Observer for Recorder {
        fn finished(&mut self, graph: &Graph, edge: EdgeId, _: &Outcome, output: &[u8], _: Progress) {
            let output_path = graph.path(graph.edge(edge).outputs()[0]).to_string();
            self.finished.push((output_path, String::from_utf8_lossy(output).into_owned()));
        }
    }

    fn cat() -> Graphs {
        graph().rule("cat", &[("command", "cat $in > $out")])
    }

    #[test]
    fn builds_and_records() {
        let graph = cat().edge("cat", &["ab"], &["a", "b"]).edge("cat", &["abc"], &["ab", "c"]);
        let mut build = Build::new("records", graph.finish(), &[("a", "a\n"), ("b", "b\n"), ("c", "c\n")]);
        assert_eq!(build.ran(&[]), ["ab", "abc"]);
        assert_eq!(build.read("abc"), "a\nb\nc\n");
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("c", "changed\n");
        assert_eq!(build.ran(&[]), ["abc"]);
        assert_eq!(build.read("abc"), "a\nb\nchanged\n");
    }

    #[test]
    fn creates_the_directories_of_outputs() {
        let graph = graph().rule("cc", &[("command", "touch $out $out.d"), ("depfile", "deps/$out.d")])
            .edge("cc", &["out/sub/a.o"], &[]);
        let mut build = Build::new("directories", graph.finish(), &[]);
        assert_eq!(build.ran(&[]), ["out/sub/a.o"]);
        assert!(build.dir.join("deps/out/sub").is_dir());
    }

    #[test]
    fn runs_in_parallel() {
        let graph = graph().rule("sleep", &[("command", "sleep 0.4 && touch $out")])
            .edge("sleep", &["a"], &[]).edge("sleep", &["b"], &[]).edge("sleep", &["c"], &[]);
        let mut build = Build::new("parallel", graph.finish(), &[]);
        build.options.jobs = 3;
        let start = Instant::now();
        assert_eq!(build.ran(&[]).len(), 3);
        assert!(start.elapsed() < Duration::from_millis(1150), "{:?}", start.elapsed());
    }

    #[test]
    fn captures_output() {
        let graph = graph().rule("say", &[("command", "echo out && echo err >&2 && touch $out")]);
        let graph = graph.edge("say", &["a"], &[]);
        let mut build = Build::new("output", graph.finish(), &[]);
        assert_eq!(build.build(&[]).1, [("a".to_string(), "out\nerr\n".to_string())]);
    }

    #[test]
    fn stops_at_a_failure() {
        let graph = cat().rule("fail", &[("command", "echo failing && exit 3")]).edge("fail", &["bad"], &[])
            .edge("cat", &["after"], &["bad"]).edge("cat", &["other"], &["a"]).edge("cat", &["more"], &["a"]);
        let mut build = Build::new("failure", graph.finish(), &[("a", "a\n")]);
        build.options.jobs = 1;
        let (report, finished) = build.build(&[]);
        assert_eq!(finished, [("bad".to_string(), "failing\n".to_string())]);
        assert_eq!(report, Report { ran: 1, failed: 1, not_run: 3, ..Report::default() });
        // with `-k 0`, what does not depend on the failure is built
        build.options.keep_going = 0;
        let (report, finished) = build.build(&[]);
        assert_eq!(finished.len(), 3);
        assert_eq!(report, Report { ran: 3, failed: 1, not_run: 1, ..Report::default() });
        // the failed edge is not recorded, so it runs again
        assert_eq!(build.build(&[]).0, Report { ran: 1, failed: 1, not_run: 1, ..Report::default() });
    }

    #[test]
    fn records_discovered_inputs() {
        let graph = graph()
            .rule("cc", &[
                ("command", "cat $in > $out && echo \"$out: $in extra\\ file.h\" > $out.d"),
                ("depfile", "$out.d"),
                ("deps", "gcc"),
            ])
            .edge("cc", &["a.o"], &["a.c"]);
        let mut build = Build::new("depfile", graph.finish(), &[("a.c", "a\n"), ("extra file.h", "h\n")]);
        assert_eq!(build.ran(&[]), ["a.o"]);
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("extra file.h", "changed\n");
        assert_eq!(build.ran(&[]), ["a.o"]);
    }

    #[test]
    fn builds_again_when_a_variable_it_read_changes() {
        // as rustc lists the variables `env!` reads, in the environment the command is given
        let graph = graph()
            .rule("cc", &[
                ("command", "echo \"$$RIG_TEST_VARIABLE\" > $out && printf '$out:\\n# env-dep:RIG_TEST_VARIABLE\\n' > $out.d"),
                ("depfile", "$out.d"),
            ])
            .edge("cc", &["a.o"], &[]);
        let mut build = Build::new("variables", graph.finish(), &[]);
        let mut set = |value: Option<&str>| {
            build.options.env.retain(|(name, _)| name != "RIG_TEST_VARIABLE");
            build.options.env.extend(value.map(|value| ("RIG_TEST_VARIABLE".to_string(), value.to_string())));
            build.ran(&[])
        };
        assert_eq!(set(Some("one")), ["a.o"]);
        assert_eq!(set(Some("one")), Vec::<String>::new());
        assert_eq!(set(Some("two")), ["a.o"]);
        assert_eq!(build.read("a.o"), "two\n");
        // unset, where the variable rig is run with is not
        if std::env::var_os("RIG_TEST_VARIABLE").is_none() {
            let mut stamps = Stamps::new(&build.graph, &build.dir);
            let plan = plan::plan(&build.graph, &build.state, &mut stamps, &[]).unwrap();
            assert_eq!(plan.edges[0].1, Reason::EnvChanged("RIG_TEST_VARIABLE".into()));
        }
    }

    #[test]
    fn fails_with_an_invalid_depfile() {
        let graph = graph().rule("cc", &[("command", "touch $out && echo nonsense > $out.d"), ("depfile", "$out.d")])
            .edge("cc", &["a.o"], &[]);
        let mut build = Build::new("bad-depfile", graph.finish(), &[]);
        assert_eq!(build.build(&[]).0.failed, 1);
    }

    #[test]
    fn restat_skips_past_order_only_inputs() {
        // `out` depends on `gen.h`, which a restat edge leaves alone, and on `first` only to be built after it, which
        // does change, so `out` is up to date, as the differential tests found
        let graph = cat()
            .rule("gen", &[
                ("command", "cat $in > $out.tmp && (cmp -s $out.tmp $out || mv $out.tmp $out) && rm -f $out.tmp"),
                ("restat", "1"),
            ])
            .edge("cat", &["first"], &["a"])
            .edge("gen", &["gen.h"], &["gen.in", "|", "first"])
            .edge("cat", &["out"], &["gen.h", "||", "first"]);
        let mut build = Build::new("restat-order-only", graph.finish(), &[("a", "a\n"), ("gen.in", "same\n")]);
        assert_eq!(build.ran(&[]), ["first", "gen.h", "out"]);
        std::fs::remove_file(build.dir.join("first")).unwrap();
        let (report, finished) = build.build(&[]);
        assert_eq!(finished.into_iter().map(|(output, _)| output).collect::<Vec<_>>(), ["first", "gen.h"]);
        assert_eq!(report.restat_skipped, 1);
    }

    #[test]
    fn restat_skips_what_did_not_change() {
        // the generated file is only written when its contents change, as jig writes generated files
        let graph = cat()
            .rule("gen", &[
                ("command", "cat $in > $out.tmp && (cmp -s $out.tmp $out || mv $out.tmp $out) && rm -f $out.tmp"),
                ("restat", "1"),
            ])
            .edge("gen", &["gen.h"], &["gen.in"]).edge("cat", &["a.o"], &["gen.h"]).edge("cat", &["app"], &["a.o"]);
        let mut build = Build::new("restat", graph.finish(), &[("gen.in", "same\n")]);
        assert_eq!(build.ran(&[]), ["gen.h", "a.o", "app"]);
        // the input changes without changing what is generated, so only the generator runs
        build.write("gen.in", "same\n");
        let file = std::fs::File::options().write(true).open(build.dir.join("gen.in")).unwrap();
        file.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
        let (report, finished) = build.build(&[]);
        assert_eq!(finished.into_iter().map(|(output, _)| output).collect::<Vec<_>>(), ["gen.h"]);
        assert_eq!(report.restat_skipped, 2);
        // and once it has, nothing is out of date
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        // while a change to what is generated builds what depends on it
        build.write("gen.in", "different\n");
        assert_eq!(build.ran(&[]), ["gen.h", "a.o", "app"]);
    }

    #[test]
    fn console_edges_run_alone() {
        // a console edge fails if another is running, as the directory exists
        let graph = graph()
            .rule("alone", &[
                ("command", "mkdir running && sleep 0.1 && rmdir running && touch $out"),
                ("pool", "console"),
            ])
            .edge("alone", &["a"], &[]).edge("alone", &["b"], &[]).edge("alone", &["c"], &[]);
        let mut build = Build::new("console", graph.finish(), &[]);
        build.options.jobs = 3;
        let (report, _) = build.build(&[]);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.ran, 3);
    }

    #[test]
    fn shows_progress() {
        let graph = cat().rule("fail", &[("command", "echo why && false"), ("description", "FAIL $out")])
            .edge("cat", &["ab"], &["a", "b"]).edge("fail", &["bad"], &[]);
        let mut build = Build::new("console-output", graph.finish(), &[("a", "a\n"), ("b", "b\n")]);
        build.options.jobs = 1;
        let mut console = Console { out: Vec::new(), quiet: false, verbose: false };
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        let out = String::from_utf8(console.out).unwrap();
        assert_eq!(out, "[1/2] cat a b > ab\n[2/2] FAIL bad\nFAILED: bad\necho why && false\nwhy\n");
    }

    #[test]
    fn cancelling_stops_the_commands() {
        // a command whose process starts another, which is in its process group
        let graph = graph().rule("wait", &[("command", "sleep 30 & echo $$! > $out.pid; wait; touch $out")])
            .edge("wait", &["a"], &[]).edge("wait", &["b"], &[]);
        let mut build = Build::new("cancel", graph.finish(), &[]);
        build.options.jobs = 2;
        let cancel = build.options.cancel.clone();
        let start = Instant::now();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::SeqCst);
        });
        let (report, _) = build.build(&[]);
        canceller.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
        assert!(report.interrupted && !report.succeeded(), "{report:?}");
        // the edges which were stopped are not failures
        assert_eq!((report.ran, report.failed), (2, 0));
        // the sleeps were stopped too, rather than left running
        for pid in ["a.pid", "b.pid"] {
            // a process which has stopped may not have been reaped yet, as its parent is now the system's
            let pid = build.read(pid);
            let ps = Command::new("ps").args(["-o", "stat=", "-p", pid.trim()]).output().unwrap();
            let state = String::from_utf8_lossy(&ps.stdout);
            let stopped = state.trim().is_empty() || state.trim().starts_with('Z');
            assert!(stopped, "{} is still running: {state}", pid.trim());
        }
        assert!(!build.dir.join("a").exists());
    }

    #[test]
    fn commands_share_the_jobs() {
        // the only edge running takes the tokens of the other jobs, as a compiler's threads would, and gives them back
        let graph = graph()
            .rule("hold", &[("command", "f=$${MAKEFLAGS#*fifo:}; head -c 2 \"$$f\" > $out && printf ++ > \"$$f\"")])
            .edge("hold", &["a"], &[]);
        let mut build = Build::new("jobserver", graph.finish(), &[]);
        build.options.jobs = 3;
        assert_eq!(build.ran(&[]), ["a"]);
        assert_eq!(build.read("a"), "++");
    }

    #[test]
    fn runs_without_a_jobserver() {
        let graph = graph().rule("check", &[("command", "echo \"[$$MAKEFLAGS]\" > $out")]).edge("check", &["a"], &[]);
        let mut build = Build::new("no-jobserver", graph.finish(), &[]);
        build.options.jobserver = false;
        // a jobserver which rig is run by is in the environment either way
        if std::env::var_os("MAKEFLAGS").is_none() {
            build.ran(&[]);
            assert_eq!(build.read("a"), "[]\n");
        }
    }
}
