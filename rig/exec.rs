//! Running a build: the edges a [`plan`](crate::plan::plan) finds must run, in parallel, each once those it depends on
//! have run.
//!
//! Each edge runs its command with `sh -c` in the build's directory, on one of a thread for each of the
//! [`Options::jobs`], which is the most which run at once. Its output, stdout and stderr together, is captured and
//! given to an [`Observer`] once it finishes, while an edge in the console pool runs alone among console edges with the
//! terminal, and the output of the others is held until it finishes. An edge which succeeds is recorded as soon as it
//! does, with the inputs its depfile lists, so that an interrupted build loses only the edges which were running.
//!
//! The outputs of a restat edge are found again once it has run, and if none of them changed, an edge which would run
//! only because it depends on them is checked again, and does not run if it is otherwise up to date.
//!
//! A cached edge, when the build has a [`Cache`], is looked up in it before it runs, by its action and the contents of
//! its inputs, and if it has run with them before, its outputs and depfile are restored from the cache, and what its
//! command printed is shown, rather than running it. Otherwise its outputs, depfile and what it printed are added to the
//! cache once it has run and succeeded. A cache which cannot be read or written is a miss, rather than a failure.
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

use crate::cache::{self, Cache, Hash};
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
    /// The cache the results of cached edges are kept in, if there is one.
    pub cache: Option<Cache>,
    /// The environment variables which a hermetic edge inherits beside those it passes on, which only locate the tools
    /// and files it uses, rather than changing what it produces, so they are not its inputs: `PATH`, `HOME` and
    /// `TMPDIR` by default.
    pub locating: Vec<String>,
}
impl Options {
    /// Options for a build in a directory, which stops at the first failure, with as many jobs as ninja runs by
    /// default: two more than the machine runs at once, as commands wait on files and processes, or one more with two,
    /// or two with one.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let jobs = match std::thread::available_parallelism().map_or(1, std::num::NonZero::get) {
            1 => 2,
            2 => 3,
            threads => threads + 2,
        };
        Self { dir: dir.into(), jobs, keep_going: 1, jobserver: true, cancel: Arc::default(),
            env: Vec::new(),
            cache: None,
            locating: ["PATH", "HOME", "TMPDIR"].map(String::from).to_vec(),
        }
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
    /// The edges which ran whose outputs were fetched from the cache, rather than by running their commands.
    pub cached: usize,
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

/// An edge for a thread to run: its index in the plan, its action, whether it is in the console pool, and its result
/// in the cache, which is restored rather than running the action, if it has one.
struct Job {
    index: usize,
    action: Action,
    console: bool,
    restore: Option<Restore>,
    /// The variables a hermetic edge inherits, or `None` if it inherits every one.
    inherit: Option<Vec<String>>,
}

/// The result of an edge in the cache, which is restored to its outputs.
struct Restore {
    cache: Cache,
    entry: cache::Entry,
}
impl Restore {
    /// Restore the outputs of the edge, relative to the build's directory, returning what its command printed.
    ///
    /// If one cannot be, every blob of the result is checked, so that those which are corrupted are removed, and are
    /// added again once the edge has run.
    fn apply(&self, dir: &Path) -> std::io::Result<Vec<u8>> {
        let restored = self.entry.outputs.iter().try_for_each(|output| {
            self.cache.restore(&output.blob, &dir.join(&output.path), output.executable)
        });
        let printed = restored.and_then(|()| self.cache.read_blob(&self.entry.printed));
        if printed.is_err() {
            self.cache.check(&self.entry);
        }
        printed
    }
}

/// An edge which has finished.
struct Finished {
    /// The index of the edge in the plan.
    index: usize,
    outcome: Outcome,
    output: Vec<u8>,
    start: SystemTime,
    duration: std::time::Duration,
    /// Whether its outputs were restored from the cache.
    cached: bool,
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
    let priority = critical_paths(graph, state, &edges, &dependents);
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
        priority,
        inputs: HashMap::new(),
        before: HashMap::new(),
        progress: Progress::default(),
        report: Report::default(),
        console: false,
        held: Vec::new(),
        error: None,
        jobserver: None,
        tokens: HashMap::new(),
        implicit: None,
        directories: HashSet::new(),
        processes: HashMap::new(),
        keys: HashMap::new(),
        relocation: std::path::absolute(&options.dir).map(|dir| dir.display().to_string()).unwrap_or_default(),
    };
    if options.jobserver && !run.edges.is_empty() {
        run.jobserver = Jobserver::from_env().or_else(|| Jobserver::create(options.jobs.max(1)).ok());
    }
    run.status = vec![Status::Waiting; run.edges.len()];
    run.progress.total = run.edges.len();
    let ready: Vec<usize> = (0..run.edges.len()).filter(|&index| run.waiting[index] == 0).collect();
    for index in ready {
        run.make_ready(index);
    }
    run.run(observer)?;
    run.report.not_run = run.status.iter().filter(|status| matches!(status, Status::Waiting)).count();
    Ok(run.report)
}

/// Returns the priority of each edge in the plan: how long it took when it last ran, in milliseconds, or one if it has
/// not run, with the longest of the edges which depend on it, as the time of the longest path from it to the end of the
/// build. Edges on that path, the critical one, run first, so that a long chain does not start last.
///
/// The plan is in an order in which each edge runs after those it depends on, so the priorities are found from its end.
fn critical_paths(graph: &Graph, state: &State, edges: &[EdgeId], dependents: &[Vec<usize>]) -> Vec<u64> {
    let mut priority = vec![0; edges.len()];
    for index in (0..edges.len()).rev() {
        let record = state.get(&plan::first_output(graph, edges[index]));
        let duration = record.map_or(1, |record| u64::from(record.duration).max(1));
        let longest = dependents[index].iter().map(|&dependent| priority[dependent]).max().unwrap_or(0);
        priority[index] = duration.saturating_add(longest);
    }
    priority
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
    /// The edges which are ready to run, by their priority, highest first, then their order in the plan.
    ready: BTreeSet<(std::cmp::Reverse<u64>, usize)>,
    /// The priority of each edge, the time of the longest path from it to the end of the build.
    priority: Vec<u64>,
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
    /// The edge running in the job the build holds without a token, if one is.
    implicit: Option<usize>,
    /// The directories of outputs which have been created, relative to the build's directory.
    directories: HashSet<PathBuf>,
    /// The process group of each running edge which is not in the console pool.
    processes: HashMap<usize, u32>,
    /// The first key in the cache of each running cached edge.
    keys: HashMap<usize, Hash>,
    /// The build's directory, absolute, which is replaced in the actions of cached edges by a marker in their keys, so
    /// that a build in another directory shares their results, or empty if it is not known.
    relocation: String,
}

impl Run<'_, '_> {
    fn run(&mut self, observer: &mut dyn Observer) -> Result<(), Error> {
        let (sender, receiver) = std::sync::mpsc::channel::<Message>();
        let interrupts = sys::Interrupts::catch();
        let makeflags = self.jobserver.as_ref().and_then(Jobserver::makeflags).map(String::from);
        // when the build was interrupted, until its processes are killed
        let mut interrupted_at = None;
        // the edges are run by a thread for each job, rather than a thread for each edge
        let (jobs, work) = std::sync::mpsc::channel::<Job>();
        let work = std::sync::Mutex::new(work);
        let (dir, env) = (self.options.dir.clone(), self.options.env.clone());
        std::thread::scope(|scope| {
            for _ in 0..self.options.jobs.max(1) {
                let (work, sender, dir, env, makeflags) = (&work, sender.clone(), &dir, &env, makeflags.as_deref());
                scope.spawn(move || {
                    // the lock is held while waiting for a job, which the other threads wait for in turn
                    while let Ok(Ok(Job { index, action, console, restore, inherit })) = work.lock().map(|work| work.recv()) {
                        let start = SystemTime::now();
                        let clock = Instant::now();
                        // a result which cannot be restored, as the cache is corrupted, is a miss, and the action runs
                        let restored = restore.and_then(|restore| restore.apply(dir).ok());
                        let cached = restored.is_some();
                        let (outcome, output) = if let Some(printed) = restored {
                            (Outcome::Succeeded, printed)
                        } else {
                            let spawned = |pid| {
                                let _ = sender.send(Message::Spawned(index, pid));
                            };
                            let environment = Environment { makeflags, env, inherit: inherit.as_deref() };
                            run_command(&action, dir, console, &environment, &spawned)
                        };
                        let duration = clock.elapsed();
                        let finished = Finished { index, outcome, output, start, duration, cached };
                        let _ = sender.send(Message::Finished(finished));
                    }
                });
            }
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
                    // an edge runs in the job the build holds without a token if it is free, and any other takes a
                    // token
                    match &self.jobserver {
                        Some(jobserver) if self.implicit.is_some() => {
                            let Some(token) = jobserver.try_acquire() else {
                                self.make_ready(index);
                                waiting_for_token = true;
                                break;
                            };
                            self.tokens.insert(index, token);
                        },
                        _ => self.implicit = Some(index),
                    }
                    self.start(index, observer);
                    let console = self.graph.edge(self.edges[index]).pool() == Pool::Console;
                    let action = self.action(index).clone();
                    let restore = self.lookup(index);
                    let edge = self.graph.edge(self.edges[index]);
                    let inherit = edge.hermetic().then(|| {
                        let mut inherit = self.options.locating.clone();
                        inherit.extend(edge.pass_env().iter().cloned());
                        inherit
                    });
                    // a thread for each job takes it, as no more run than there are jobs
                    let _ = jobs.send(Job { index, action, console, restore, inherit });
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
                        if self.implicit == Some(finished.index) {
                            self.implicit = None;
                        }
                        self.finish(finished, observer);
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // the threads stop once there are no more jobs
            drop(jobs);
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
    /// Returns the edge of the highest priority which is ready to run, other than one in the console pool while
    /// another runs.
    fn next_ready(&mut self) -> Option<usize> {
        let entry = *self.ready.iter().find(|&&(_, index)| {
            !self.console || self.graph.edge(self.edges[index]).pool() != Pool::Console
        })?;
        self.ready.remove(&entry);
        Some(entry.1)
    }
    fn make_ready(&mut self, index: usize) {
        self.ready.insert((std::cmp::Reverse(self.priority[index]), index));
    }
    fn action(&self, index: usize) -> &Action {
        match self.graph.edge(self.edges[index]).kind() {
            EdgeKind::Command(action) => action,
            // the plan has no aliases
            EdgeKind::Alias => unreachable!("an alias is not run"),
        }
    }
    /// Whether an edge which would run only as an input is built again is up to date after all, as a restat or hashed
    /// edge it depends on left its outputs alone, or as it is hashed and its inputs' contents are as they were.
    ///
    /// It is checked again once the edges it depends on have run, if one of them left its outputs alone, by the inputs
    /// which make it run, so that another which it depends on only through an order-only input may have changed.
    ///
    /// A hashed edge is checked again whatever the edges it depends on did, as it compares the contents of its inputs,
    /// rather than whether they were built again.
    fn restat_clean(&mut self, index: usize) -> bool {
        let unchanged = |dep: &usize| self.status[*dep] == Status::Done { changed: false };
        let hashed = self.graph.edge(self.edges[index]).hashed();
        let rebuilt = matches!(self.reasons[index], Reason::InputRebuilt(_));
        if !rebuilt || !(hashed || self.deps[index].iter().any(unchanged)) {
            return false;
        }
        let (graph, index_of, status) = (self.graph, &self.index, &self.status);
        let changed = |edge: EdgeId| !hashed && changed(graph, edge, index_of, status, &mut HashSet::new());
        plan::reason(graph, self.state, &mut self.stamps, self.edges[index], &changed).is_none()
    }
    fn start(&mut self, index: usize, observer: &mut dyn Observer) {
        let edge = self.edges[index];
        self.inputs.insert(index, plan::input_stamps(self.graph, self.state, edge, &mut self.stamps));
        let data = self.graph.edge(edge);
        // the directories of the outputs and depfile are created first, as ninja creates them, which the command
        // fails without if it needs them
        let outputs = data.outputs().iter().chain(data.implicit_outputs()).map(|&node| self.graph.path(node));
        for path in outputs.chain(data.depfile()) {
            // each directory once, as many outputs share one
            if let Some(dir) = Path::new(path).parent().filter(|dir| !dir.as_os_str().is_empty())
                && self.directories.insert(dir.to_path_buf())
            {
                let _ = std::fs::create_dir_all(self.options.dir.join(dir));
            }
        }
        // the outputs of a restat or hashed edge are stamped before it runs, to find whether it changed them, by their
        // contents if it is hashed, reusing its last record's hashes
        if data.restat() || data.hashed() {
            let previous = self.state.get(&plan::first_output(self.graph, edge));
            let outputs = data.outputs().iter().chain(data.implicit_outputs());
            let before = outputs
                .map(|&output| {
                    let path = self.graph.path(output);
                    let recorded = previous.and_then(|record| {
                        let (_, stamp) = record.outputs.iter().find(|(recorded, _)| recorded == path)?;
                        Some((stamp, record.start.saturating_add(u64::from(record.duration))))
                    });
                    self.stamps.node_as(output, data.hashed(), recorded)
                })
                .collect();
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
        let Finished { index, mut outcome, output, start, mut duration, cached } = finished;
        let edge = self.edges[index];
        let data = self.graph.edge(edge);
        self.progress.running -= 1;
        self.progress.finished += 1;
        self.report.ran += 1;
        let inputs = self.inputs.remove(&index).unwrap_or_default();
        let key = self.keys.remove(&index);
        if cached {
            self.report.cached += 1;
            // the time it took when it ran is kept, as what it would take if it is not in the cache next time
            let previous = self.state.get(&plan::first_output(self.graph, edge));
            if let Some(previous) = previous {
                duration = Duration::from_millis(u64::from(previous.duration));
            }
        }
        if outcome.succeeded() {
            let discovered = match data.depfile() {
                Some(path) => read_depfile(&self.options.dir.join(path)),
                None => Ok(depfile::Discovered::default()),
            };
            match discovered {
                Ok(discovered) => {
                    let record = plan::record(self.graph, edge, inputs, &discovered, &mut self.stamps, start, duration);
                    if let Some(key) = key.filter(|_| !cached) {
                        // the cache is only an aid, so an edge which cannot be added to it is not a failure
                        let _ = self.store(index, &key, &record, &output);
                    }
                    let changed = match self.before.remove(&index) {
                        Some(before) => {
                            !record.outputs.iter().zip(&before).all(|((_, after), before)| after.same(*before))
                        },
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
    /// Returns the result of a cached edge in the cache, if it has run with the same action and inputs before, finding
    /// its first key, which its result is added to the cache by if it runs.
    ///
    /// Each set of inputs the edge discovered with that key, the latest first, is stamped by their contents as they
    /// are now, which leads to the result with those contents if there is one.
    fn lookup(&mut self, index: usize) -> Option<Restore> {
        let cache = self.options.cache.as_ref()?;
        let edge = self.graph.edge(self.edges[index]);
        let identity = edge.cache()?;
        if edge.pool() == Pool::Console {
            return None;
        }
        let outputs = edge.outputs().iter().chain(edge.implicit_outputs());
        let outputs: Vec<&str> = outputs.map(|&node| self.graph.path(node)).collect();
        let inputs = self.inputs.get(&index)?;
        let inputs: Vec<(&str, Option<&Hash>)> = inputs.iter().map(|(path, stamp)| (path.as_str(), stamp.hash())).collect();
        let action = plan::relocated_action_bytes(edge, &self.relocation);
        let values: Vec<_> = edge.pass_env().iter().map(|name| self.stamps.env_hash(name)).collect();
        let passed: Vec<_> = edge.pass_env().iter().zip(&values).map(|(name, hash)| (name.as_str(), hash.as_ref())).collect();
        let key = cache::first_key(identity, &action, &self.options.env, &passed, &outputs, edge.depfile(), &inputs);
        self.keys.insert(index, key);
        for discovery in cache.manifest(&key) {
            let stamps: Vec<Stamp> = discovery.inputs.iter().map(|path| self.stamps.path_as(path, true, None)).collect();
            let inputs = discovery.inputs.iter().zip(&stamps).map(|(path, stamp)| (path.as_str(), stamp.hash()));
            let inputs: Vec<_> = inputs.collect();
            let values: Vec<_> = discovery.env.iter().map(|name| self.stamps.env_hash(name)).collect();
            let env = discovery.env.iter().zip(&values).map(|(name, hash)| (name.as_str(), hash.as_ref()));
            let second = cache::second_key(&key, &inputs, &env.collect::<Vec<_>>());
            if let Some(entry) = cache.result(&second) {
                cache.used(&key, &second);
                return Some(Restore { cache: cache.clone(), entry });
            }
        }
        None
    }
    /// Add the result of a cached edge which has run to the cache, by its first key: its outputs, its depfile and what
    /// its command printed, which it is restored from by its second key, of the contents of the inputs it discovered,
    /// which are added to the first key's manifest.
    ///
    /// An edge any of whose outputs is not a file it wrote is not added.
    fn store(&mut self, index: usize, key: &Hash, record: &crate::state::Record, printed: &[u8]) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let Some(cache) = &self.options.cache else {
            return Ok(());
        };
        let edge = self.graph.edge(self.edges[index]);
        let mut outputs = Vec::new();
        for (path, stamp) in &record.outputs {
            let Some(hash) = stamp.hash() else {
                return Ok(());
            };
            let full = self.options.dir.join(path);
            let executable = std::fs::metadata(&full)?.permissions().mode() & 0o111 != 0;
            cache.put_file(&full, hash)?;
            outputs.push(cache::Output { path: path.clone(), blob: *hash, executable });
        }
        if let Some(depfile) = edge.depfile() {
            let full = self.options.dir.join(depfile);
            if full.is_file() {
                let hash = crate::blake3::hash_file(&full)?;
                cache.put_file(&full, &hash)?;
                outputs.push(cache::Output { path: depfile.to_string(), blob: hash, executable: false });
            }
        }
        let printed = cache.put_bytes(printed)?;
        let inputs: Vec<_> = record.discovered.iter().map(|(path, stamp)| (path.as_str(), stamp.hash())).collect();
        let values: Vec<_> = record.env.iter().map(|(name, _)| self.stamps.env_hash(name)).collect();
        let env: Vec<_> = record.env.iter().zip(&values).map(|((name, _), hash)| (name.as_str(), hash.as_ref())).collect();
        let second = cache::second_key(key, &inputs, &env);
        cache.put_result(&second, &cache::Entry { outputs, printed })?;
        let discovery = cache::Discovery {
            inputs: record.discovered.iter().map(|(path, _)| path.clone()).collect(),
            env: record.env.iter().map(|(name, _)| name.clone()).collect(),
        };
        cache.add_to_manifest(key, discovery.clone())?;
        // and to the remote cache, if it takes pushes, which is only an aid too
        let _ = cache.push(key, &second, &discovery);
        Ok(())
    }
    /// Mark an edge as done, making ready the edges which were waiting only for it.
    fn done(&mut self, index: usize, changed: bool) {
        self.status[index] = Status::Done { changed };
        for dependent in self.dependents[index].clone() {
            self.waiting[dependent] -= 1;
            if self.waiting[dependent] == 0 {
                self.make_ready(dependent);
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
    /// The variables rig is run with which a hermetic command inherits, or `None` if it inherits every one.
    inherit: Option<&'a [String]>,
}

/// Run an action in a directory, returning how it finished and its output, stdout and stderr together, with the
/// jobserver's `MAKEFLAGS` if there is one, telling `spawned` the id of its process once it has started.
///
/// A shell action runs with `sh -c`, and a program runs directly, in its working directory, relative to `dir`, with
/// stderr written to its file if it has one rather than captured.
///
/// A console command has the terminal, and no output is captured, and is in rig's process group, so that it is sent
/// what the terminal sends, such as `SIGINT`. Any other has no stdin, and leads a process group of its own, which the
/// build signals when it is interrupted.
fn run_command(
    action: &Action,
    dir: &Path,
    console: bool,
    environment: &Environment,
    spawned: &dyn Fn(u32),
) -> (Outcome, Vec<u8>) {
    use std::os::unix::process::CommandExt;
    let (mut command, program, stderr_file) = match action {
        Action::Shell(line) => {
            let mut sh = Command::new("/bin/sh");
            sh.arg("-c").arg(line).current_dir(dir);
            (sh, "sh", None)
        },
        Action::Exec(exec) => {
            let mut program = Command::new(&exec.program);
            let working = exec.dir.as_ref().map_or_else(|| dir.to_path_buf(), |working| dir.join(working));
            program.args(&exec.args).current_dir(working);
            (program, exec.program.as_str(), exec.stderr.as_ref().map(|path| dir.join(path)))
        },
    };
    if let Some(inherit) = environment.inherit {
        command.env_clear();
        for name in inherit {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    command.envs(environment.env.iter().map(|(name, value)| (name, value)));
    if let Action::Exec(exec) = action {
        command.envs(exec.env.iter().map(|(name, value)| (name, value)));
    }
    if let Some(makeflags) = environment.makeflags {
        command.env("MAKEFLAGS", makeflags).env("CARGO_MAKEFLAGS", makeflags);
    }
    let stderr_file = match stderr_file.map(std::fs::File::create).transpose() {
        Ok(file) => file,
        Err(error) => return (Outcome::Error(format!("failed to create the file of stderr: {error}")), Vec::new()),
    };
    let failed = |error: std::io::Error| (Outcome::Error(format!("failed to run `{program}`: {error}")), Vec::new());
    if console {
        if let Some(file) = stderr_file {
            command.stderr(file);
        }
        let child = command.spawn().and_then(|mut child| {
            spawned(child.id());
            child.wait()
        });
        return match child {
            Ok(status) if status.success() => (Outcome::Succeeded, Vec::new()),
            Ok(status) => (Outcome::Failed(status), Vec::new()),
            Err(error) => failed(error),
        };
    }
    let pipe = std::io::pipe().and_then(|(reader, writer)| Ok((reader, writer.try_clone()?, writer)));
    let (mut reader, stdout, stderr) = match pipe {
        Ok(pipe) => pipe,
        Err(error) => return (Outcome::Error(format!("failed to create a pipe: {error}")), Vec::new()),
    };
    command.stdin(Stdio::null()).stdout(stdout).process_group(0);
    // a writer of the pipe which is not given to the command is closed, so that the output ends
    match stderr_file {
        Some(file) => {
            drop(stderr);
            command.stderr(file)
        },
        None => command.stderr(stderr),
    };
    let child = command.spawn();
    // the command holds the pipe's writers, which must be closed for the output to end
    drop(command);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return failed(error),
    };
    spawned(child.id());
    let mut output = Vec::new();
    let read = reader.read_to_end(&mut output);
    let outcome = match (child.wait(), read) {
        (Ok(status), Ok(_)) if status.success() => Outcome::Succeeded,
        (Ok(status), Ok(_)) => Outcome::Failed(status),
        (Err(error), _) | (_, Err(error)) => Outcome::Error(format!("failed to wait for `{program}`: {error}")),
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
            (description, EdgeKind::Command(action)) if description.is_empty() || self.verbose => action.command_line(),
            (description, _) => description.to_string(),
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
            if let EdgeKind::Command(action) = data.kind() {
                let _ = writeln!(self.out, "{}", action.command_line());
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
    use crate::graph::Exec;
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
    fn runs_the_critical_path_first() {
        // one job, so that the order they finish in is the order they start in
        let sleep = |graph: Graphs, output: &str, time: &str, inputs: &[&str]| {
            graph.edge_with("sleep", &[output], inputs, &[("time", time)])
        };
        let rules = || graph().rule("sleep", &[("command", "sleep $time && touch $out")]);
        let chain = sleep(sleep(sleep(rules(), "short", "0", &["in"]), "long", "0", &["in"]), "after", "0", &["long"]);
        let mut build = Build::new("critical-path", chain.finish(), &[("in", "1\n")]);
        build.options.jobs = 1;
        // before any edge has run, the longer path is taken first, from `long` to `after`
        assert_eq!(build.ran(&[]), ["long", "short", "after"]);
        // then by how long each took when it last ran, where `slow` took longer
        let pair = sleep(sleep(rules(), "quick", "0", &["in"]), "slow", "0.2", &["in"]);
        let mut build = Build::new("durations", pair.finish(), &[("in", "1\n")]);
        build.options.jobs = 1;
        assert_eq!(build.ran(&[]), ["quick", "slow"]);
        build.write("in", "2\n");
        assert_eq!(build.ran(&[]), ["slow", "quick"]);
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

    /// A graph of one edge, `out`, which runs a program directly.
    fn program(exec: Exec) -> Graph {
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(Action::Exec(exec));
        graph.add_edge(crate::Edge { kind, outputs: vec![out], ..Default::default() }).unwrap();
        graph
    }

    #[test]
    fn runs_programs_without_a_shell() {
        // the arguments reach the program as they are, as no shell splits or expands them, in its working directory,
        // with its variables, and its stderr in a file
        let script = "printf '%s|' \"$@\" > ../out && printf '%s' \"$VALUE\" >> ../out && echo to-stderr >&2";
        let exec = Exec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into(), "a b".into(), "$HOME".into(), "it's".into()],
            env: vec![("VALUE".into(), "v'1 2".into())],
            dir: Some("sub".into()),
            stderr: Some("err.txt".into()),
        };
        let mut build = Build::new("exec", program(exec), &[]);
        std::fs::create_dir_all(build.dir.join("sub")).unwrap();
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(build.read("out"), "a b|$HOME|it's|v'1 2");
        assert_eq!(build.read("err.txt"), "to-stderr\n");
        // with nothing captured from stderr
        assert_eq!(finished, [("out".to_string(), String::new())]);
        assert!(build.build(&[]).1.is_empty());
    }

    #[test]
    fn shows_a_failed_program_as_a_command_line() {
        let exec = Exec {
            program: "false".into(),
            args: vec!["a b".into(), "c".into()],
            env: vec![("X".into(), "1".into())],
            ..Default::default()
        };
        let mut build = Build::new("exec-failed", program(exec), &[]);
        let mut console = Console { out: Vec::new(), quiet: true, verbose: false };
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        assert_eq!(String::from_utf8(console.out).unwrap(), "FAILED: out\nX=1 false 'a b' c\n");
        // and a program which is not found fails
        let exec = Exec { program: "rig-no-such-program".into(), ..Default::default() };
        let mut build = Build::new("exec-missing", program(exec), &[]);
        let (report, finished) = build.build(&[]);
        assert_eq!(report.failed, 1, "{finished:?}");
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
                (
                    "command",
                    "echo \"$$RIG_TEST_VARIABLE\" > $out && printf '$out:\\n# env-dep:RIG_TEST_VARIABLE\\n' > $out.d",
                ),
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

    /// A graph whose compiled edge is cached by `identity`, which prints, discovers `extra.h`, and is followed by an edge
    /// which is not cached.
    fn cached(identity: &str) -> Graph {
        graph()
            .rule("cat", &[("command", "cat $in > $out")])
            .edge("cat", &["after"], &["a.o"])
            .cached(identity)
            .rule("cc", &[
                ("command", "cat $in extra.h > $out && echo \"$out: $in extra.h\" > $out.d && echo compiled"),
                ("depfile", "$out.d"),
            ])
            .edge("cc", &["a.o"], &["a.c"])
            .finish()
    }

    /// A temporary directory for a cache, which is removed when it is dropped.
    struct TempCache(Cache);
    impl TempCache {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rig-exec-cache-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(Cache::open(&dir).unwrap())
        }
    }
    impl Drop for TempCache {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.dir());
        }
    }

    #[test]
    fn fetches_from_the_cache() {
        let cache = TempCache::new("fetches");
        let files = [("a.c", "a\n"), ("extra.h", "h\n")];
        let mut first = Build::new("cache-first", cached("toolchain"), &files);
        first.options.cache = Some(cache.0.clone());
        let (report, finished) = first.build(&[]);
        assert_eq!((report.ran, report.cached), (2, 0), "{finished:?}");
        assert_eq!(finished[0], ("a.o".to_string(), "compiled\n".to_string()));
        // another build in another directory, with the same action and inputs, fetches what it printed, its outputs and
        // its depfile, while what follows runs
        let mut second = Build::new("cache-second", cached("toolchain"), &files);
        second.options.cache = Some(cache.0.clone());
        let (report, finished) = second.build(&[]);
        assert_eq!((report.ran, report.cached), (2, 1), "{finished:?}");
        assert_eq!(finished[0], ("a.o".into(), "compiled\n".into()));
        assert_eq!(second.read("a.o"), "a\nh\n");
        assert_eq!(second.read("after"), "a\nh\n");
        assert_eq!(second.read("a.o.d"), "a.o: a.c extra.h\n");
        // and records it, as if it had run
        assert_eq!(second.ran(&[]), Vec::<String>::new());
        // an input it discovered which changes is a miss, and once it is changed back, a hit
        second.write("extra.h", "changed\n");
        assert_eq!(second.build(&[]).0.cached, 0);
        assert_eq!(second.read("a.o"), "a\nchanged\n");
        second.write("extra.h", "h\n");
        assert_eq!(second.build(&[]).0.cached, 1);
        assert_eq!(second.read("a.o"), "a\nh\n");
        // as is a declared input which changes
        second.write("a.c", "changed\n");
        assert_eq!(second.build(&[]).0.cached, 0);
        second.write("a.c", "a\n");
        assert_eq!(second.build(&[]).0.cached, 1);
        // an edge of another identity, such as another toolchain, is not found
        let mut other = Build::new("cache-other", cached("other toolchain"), &files);
        other.options.cache = Some(cache.0.clone());
        assert_eq!(other.build(&[]).0.cached, 0);
        // nor is one of another environment
        let mut env = Build::new("cache-env", cached("toolchain"), &files);
        env.options.cache = Some(cache.0.clone());
        env.options.env.push(("RIG_TEST_VARIABLE".into(), "1".into()));
        assert_eq!(env.build(&[]).0.cached, 0);
    }

    #[test]
    fn fetches_from_a_remote_cache() {
        // a build pushes what it runs to the remote cache, from which a build with an empty cache of its own fetches it
        let (server, first_cache, second_cache) =
            (TempCache::new("remote-server"), TempCache::new("remote-first"), TempCache::new("remote-second"));
        let files = [("a.c", "a\n"), ("extra.h", "h\n")];
        let mut first = Build::new("remote-first", cached("toolchain"), &files);
        let remote = Arc::new(crate::remote::served(server.0.clone(), true, true));
        first.options.cache = Some(first_cache.0.clone().with_remote(remote));
        assert_eq!(first.build(&[]).0.cached, 0);
        assert_eq!(server.0.stats().results, 1);
        let mut second = Build::new("remote-second", cached("toolchain"), &files);
        let remote = Arc::new(crate::remote::served(server.0.clone(), false, true));
        second.options.cache = Some(second_cache.0.clone().with_remote(remote));
        let (report, finished) = second.build(&[]);
        assert_eq!((report.ran, report.cached), (2, 1), "{finished:?}");
        assert_eq!(finished[0], ("a.o".to_string(), "compiled\n".to_string()));
        assert_eq!(second.read("a.o"), "a\nh\n");
        assert_eq!(second.read("a.o.d"), "a.o: a.c extra.h\n");
        // which it keeps, with the blobs it fetched
        assert_eq!(second_cache.0.stats().results, 1);
    }

    #[test]
    fn runs_what_the_cache_cannot_restore() {
        let cache = TempCache::new("corrupted");
        let files = [("a.c", "a\n"), ("extra.h", "h\n")];
        let mut first = Build::new("cache-corrupt-first", cached("toolchain"), &files);
        first.options.cache = Some(cache.0.clone());
        assert_eq!(first.build(&[]).0.cached, 0);
        // every blob is corrupted
        for shard in std::fs::read_dir(cache.0.dir().join("cas")).unwrap() {
            for blob in std::fs::read_dir(shard.unwrap().path()).unwrap() {
                std::fs::write(blob.unwrap().path(), "corrupted").unwrap();
            }
        }
        let mut second = Build::new("cache-corrupt-second", cached("toolchain"), &files);
        second.options.cache = Some(cache.0.clone());
        let (report, finished) = second.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.cached), (2, 0));
        assert_eq!(second.read("a.o"), "a\nh\n");
        // and what it ran is added again
        let mut third = Build::new("cache-corrupt-third", cached("toolchain"), &files);
        third.options.cache = Some(cache.0.clone());
        assert_eq!(third.build(&[]).0.cached, 1);
        assert_eq!(third.read("a.o"), "a\nh\n");
    }

    #[test]
    fn shares_results_with_builds_in_other_directories() {
        // a program given its build's directory, as rustc is to remap it, is the same in the key of the cache wherever
        // the build is
        let cache = TempCache::new("relocated");
        let graph = |name: &str| {
            let dir = std::env::temp_dir().join(format!("rig-exec-{}-{name}", std::process::id()));
            let script = "printf '%s' \"${1#\"$2\"}\" > out";
            let exec = Exec {
                program: "sh".into(),
                args: vec!["-c".into(), script.into(), "sh".into(), format!("{}/sub", dir.display()), dir.display().to_string()],
                ..Default::default()
            };
            let mut graph = Graph::new();
            let out = graph.file("out");
            let kind = EdgeKind::Command(Action::Exec(exec));
            graph.add_edge(crate::Edge { kind, outputs: vec![out], cache: Some("id".into()), ..Default::default() }).unwrap();
            graph
        };
        let mut first = Build::new("relocated-first", graph("relocated-first"), &[]);
        first.options.cache = Some(cache.0.clone());
        assert_eq!(first.build(&[]).0.cached, 0);
        assert_eq!(first.read("out"), "/sub");
        let mut second = Build::new("relocated-second", graph("relocated-second"), &[]);
        second.options.cache = Some(cache.0.clone());
        assert_eq!(second.build(&[]).0.cached, 1);
        assert_eq!(second.read("out"), "/sub");
    }

    /// A graph of one edge, `out`, which writes `HOME`, `PATH` and `RIG_TEST_PASSED` as it sees them, passing on
    /// `pass_env`, and cached if `cached`.
    fn environment(cached: bool, pass_env: &[&str]) -> Graph {
        let script = "printf '%s|%s|%s' \"${HOME-unset}\" \"${PATH+set}\" \"${RIG_TEST_PASSED-unset}\" > out";
        let exec = Exec { program: "/bin/sh".into(), args: vec!["-c".into(), script.into()], ..Default::default() };
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(Action::Exec(exec));
        let pass_env = pass_env.iter().map(|&name| name.to_string()).collect();
        let cache = cached.then(|| "id".to_string());
        graph.add_edge(crate::Edge { kind, outputs: vec![out], cache, pass_env, ..Default::default() }).unwrap();
        graph
    }

    #[test]
    fn runs_cached_edges_in_a_hermetic_environment() {
        // the names of the variables `env` is run with, as it prints them, passing on `pass_env`, and cached if `cached`
        let names = |name: &str, cached: bool, pass_env: &[&str], locating: &[&str]| {
            let exec = Exec { program: "/usr/bin/env".into(), ..Default::default() };
            let mut graph = Graph::new();
            let out = graph.file("out");
            let kind = EdgeKind::Command(Action::Exec(exec));
            let pass_env = pass_env.iter().map(|&name| name.to_string()).collect();
            let cache = cached.then(|| "id".to_string());
            graph.add_edge(crate::Edge { kind, outputs: vec![out], cache, pass_env, ..Default::default() }).unwrap();
            let mut build = Build::new(name, graph, &[]);
            build.options.locating = locating.iter().map(|&name| name.to_string()).collect();
            build.options.jobserver = false;
            build.options.env.push(("RIG_TEST_GIVEN".into(), "given".into()));
            let (_, finished) = build.build(&[]);
            let mut names: Vec<String> =
                finished[0].1.lines().filter_map(|line| Some(line.split_once('=')?.0.to_string())).collect();
            names.sort();
            names
        };
        // a cached edge inherits only the variables which locate tools, and those it passes on, beside those the build
        // gives every command
        let both = |name: &str| std::env::var_os(name).is_some();
        let expected = |names: &[&str]| names.iter().filter(|&&name| name == "RIG_TEST_GIVEN" || both(name)).count();
        assert_eq!(names("hermetic", true, &[], &["HOME"]).len(), expected(&["HOME", "RIG_TEST_GIVEN"]));
        assert_eq!(names("hermetic-passed", true, &["PATH"], &[]), ["PATH", "RIG_TEST_GIVEN"]);
        // while one which is not cached inherits every variable
        let all = names("not-hermetic", false, &[], &[]);
        assert!(all.len() > 2 && all.iter().any(|name| name == "PATH"), "{all:?}");
    }

    #[test]
    fn builds_again_when_a_variable_it_passes_on_changes() {
        let cache = TempCache::new("passed");
        let mut build = Build::new("passed", environment(true, &["RIG_TEST_PASSED"]), &[]);
        build.options.cache = Some(cache.0.clone());
        let mut set = |value: &str| {
            build.options.env.retain(|(name, _)| name != "RIG_TEST_PASSED");
            build.options.env.push(("RIG_TEST_PASSED".into(), value.into()));
            let (report, finished) = build.build(&[]);
            assert!(report.succeeded(), "{finished:?}");
            (report.ran, report.cached)
        };
        assert_eq!(set("one"), (1, 0));
        assert_eq!(set("one"), (0, 0));
        // its value is an input, and part of the key of the cache
        assert_eq!(set("two"), (1, 0));
        assert_eq!(set("one"), (1, 1));
        let mut stamps = Stamps::new(&build.graph, &build.dir).with_env(&[("RIG_TEST_PASSED".into(), "three".into())]);
        let plan = plan::plan(&build.graph, &build.state, &mut stamps, &[]).unwrap();
        assert_eq!(plan.edges[0].1, Reason::EnvChanged("RIG_TEST_PASSED".into()));
        // and passing on another variable changes its command
        build.graph = environment(true, &["RIG_TEST_PASSED", "OTHER"]);
        let mut stamps = Stamps::new(&build.graph, &build.dir).with_env(&build.options.env);
        let plan = plan::plan(&build.graph, &build.state, &mut stamps, &[]).unwrap();
        assert_eq!(plan.edges[0].1, Reason::CommandChanged);
    }

    #[test]
    fn caches_programs_as_executable() {
        use std::os::unix::fs::PermissionsExt;
        let cache = TempCache::new("executable");
        let exec = Exec {
            program: "sh".into(),
            args: vec!["-c".into(), "printf '#!/bin/sh\\n' > out && chmod +x out".into()],
            ..Default::default()
        };
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(Action::Exec(exec));
        let edge = crate::Edge { kind, outputs: vec![out], cache: Some("id".into()), ..Default::default() };
        graph.add_edge(edge).unwrap();
        let mut first = Build::new("cache-executable-first", graph.clone(), &[]);
        first.options.cache = Some(cache.0.clone());
        assert_eq!(first.build(&[]).0.cached, 0);
        let mut second = Build::new("cache-executable-second", graph, &[]);
        second.options.cache = Some(cache.0.clone());
        assert_eq!(second.build(&[]).0.cached, 1);
        let mode = std::fs::metadata(second.dir.join("out")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
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
    fn hashed_edges_cut_off_what_does_not_change() {
        // the first character of the input is generated, so a change to the rest leaves the generated file as it was,
        // though it is written again
        let graph = cat()
            .rule("first", &[("command", "cut -c1 $in > $out")])
            .hashed()
            .edge("first", &["gen.h"], &["gen.in"])
            .edge("cat", &["a.o"], &["gen.h"])
            .edge("cat", &["app"], &["a.o"]);
        let mut build = Build::new("cutoff", graph.finish(), &[("gen.in", "abc\n")]);
        assert_eq!(build.ran(&[]), ["gen.h", "a.o", "app"]);
        build.write("gen.in", "axy\n");
        let (report, finished) = build.build(&[]);
        assert_eq!(finished.into_iter().map(|(output, _)| output).collect::<Vec<_>>(), ["gen.h"]);
        assert_eq!(report.restat_skipped, 2);
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        // while a change to what is generated builds what depends on it
        build.write("gen.in", "bxy\n");
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
    fn reuses_the_job_without_a_token() {
        // `a` runs in the job without a token and finishes first, which `c` then runs in, rather than waiting for `b`
        // to give back the only token
        let graph = graph()
            .rule("sleep", &[("command", "sleep $time && touch $out")])
            .edge_with("sleep", &["a"], &[], &[("time", "0.05")])
            .edge_with("sleep", &["b"], &[], &[("time", "0.6")])
            .edge_with("sleep", &["c"], &[], &[("time", "0.3")]);
        let mut build = Build::new("implicit-job", graph.finish(), &[]);
        build.options.jobs = 2;
        let start = Instant::now();
        assert_eq!(build.ran(&[]).len(), 3);
        assert!(start.elapsed() < Duration::from_millis(800), "{:?}", start.elapsed());
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
