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
//! A cached edge whose inputs are known from when it last ran, or which has an action which discovers them without
//! running it, may run on a worker, one of [`Options::workers`], once
//! the build's own jobs are full, if the worker has the edge's toolchain: its inputs, declared and discovered, are
//! sent to the worker, as [`crate::worker`] describes, which runs it, and its outputs are fetched, and restored as from
//! the cache. If it cannot, or the action fails there, it is given back to run here, in one of the build's own jobs,
//! as any other edge does, once one is free.
//!
//! An edge goes to a worker only if it is estimated to finish sooner there than here: sending its files, running it
//! there and fetching its outputs, against waiting for one of the build's own jobs, the least time one of the edges
//! which run here has left, and running it here, by how long they took when they last ran. Sending is estimated by the
//! size of its files when it last ran, but for those the worker has from this build, as it wrote or was sent them, and
//! the link's latency and bandwidth, and running it there by the worker's speed, as they are measured. Of the workers
//! which may run it, the one it would finish soonest on takes it. An
//! interrupted build has its workers interrupt what they run for it, which does not then run here.
//!
//! With [`Options::workers_only`], an edge which a worker may run does not run here, but waits for a worker, whatever
//! its jobs, as long as a worker is connecting, or has reported that it runs the edge. What no worker runs, and an
//! edge which has not run before, and has no action which discovers its inputs, still runs here, as does one which
//! fails on a worker, or which it cannot be sent.
//!
//! An edge with early outputs, as rustc's metadata of a crate, reports them as it runs, as rustc does with
//! `--json=artifacts`, and an edge which needs only them of its outputs starts once they are, rather than once it has
//! finished. What such an edge writes is read as it is written, and what rustc would have printed is shown and kept,
//! rather than its JSON, as for what it printed on a worker.
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
use crate::remote::Remote;
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
    /// The workers which cached edges may run on, once the build's own jobs are full.
    pub workers: Vec<Arc<Remote>>,
    /// Whether an edge which a worker may run waits for one, rather than running here.
    pub workers_only: bool,
    /// The releases of toolchains, by their identities, by which a worker may run an edge which may run on another
    /// host with the same release on its own, and which a worker which installs toolchains installs one by.
    pub releases: HashMap<String, crate::worker::Release>,
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
            workers: Vec::new(),
            workers_only: false,
            releases: HashMap::new(),
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
    /// The edges which ran on workers.
    pub remote: usize,
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
    /// An edge which was sent to a worker, which did not run it, or failed it, given back to run here.
    Returned(Box<Job>),
    /// An edge has written one of its early outputs, which it reported.
    Early(usize, NodeId),
}

/// An edge for a thread to run: its index in the plan, its action, whether it is in the console pool, and where its
/// result is looked for in the cache, which is restored rather than running the action, if it is found.
struct Job {
    index: usize,
    action: Action,
    console: bool,
    lookup: Option<Lookup>,
    /// The variables a hermetic edge inherits, or `None` if it inherits every one.
    inherit: Option<Vec<String>>,
    /// The worker it runs on, if it is sent to one.
    dispatch: Option<Dispatch>,
    /// The cache and first key it is marked to run here by if it succeeds, as it failed on a worker.
    mark: Option<(Cache, Hash)>,
    /// The early outputs it reports, by their paths, if it has any, as it writes what rustc writes with JSON.
    early: Option<Arc<Vec<(String, NodeId)>>>,
}

/// A cached edge sent to a worker: the worker, the cache its outputs are fetched into, and its action, with the paths
/// of its inputs, whose contents are found by the thread which runs it, and of the outputs it must write.
struct Dispatch {
    worker: Arc<Remote>,
    cache: Cache,
    /// The edge's first key, by which an edge which fails there, but not here, is marked to run here.
    key: Option<Hash>,
    /// How long it took when it last ran, by which the worker's speed is measured.
    took: Duration,
    identity: String,
    release: String,
    install: String,
    target: String,
    action: Action,
    env: Vec<(String, String)>,
    locating: Vec<String>,
    inputs: Vec<String>,
    /// The outputs it writes, explicit and implicit, and its depfile, if it has one.
    outputs: Vec<String>,
    /// A file stderr is written to, if it writes one, which it need not.
    stderr: Option<String>,
    /// For an edge which has not run, the action which writes its depfile without running it, which runs here first,
    /// and the depfile, whose inputs are sent too.
    discovery: Option<(Action, String)>,
}
/// What became of an edge sent to a worker.
enum Sent {
    /// It ran there, and printed this, once its outputs were restored.
    Ran(Vec<u8>),
    /// It ran there, and failed, or did not write every output.
    Failed,
    /// It was not run there.
    NotRun,
}

impl Dispatch {
    /// Run the edge on the worker, restoring its outputs.
    fn run(&self, dir: &Path) -> Sent {
        match self.try_run(dir) {
            Ok(printed) => Sent::Ran(printed),
            Err(sent) => sent,
        }
    }
    fn try_run(&self, dir: &Path) -> Result<Vec<u8>, Sent> {
        use std::os::unix::fs::PermissionsExt;
        let mut paths = self.inputs.clone();
        // an edge which has not run discovers its inputs here first, as it runs here otherwise, which shows why
        if let Some((action, depfile)) = &self.discovery {
            let environment =
                Environment { makeflags: None, env: &self.env, inherit: Some(&self.locating), reports: None };
            let (outcome, _) = run_command(action, dir, false, &environment, &|_| ());
            if !outcome.succeeded() {
                return Err(Sent::NotRun);
            }
            let found = read_depfile(&dir.join(depfile)).map_err(|_| Sent::NotRun)?;
            if !found.inputs.iter().all(|path| crate::worker::ups(path).is_some()) {
                return Err(Sent::NotRun);
            }
            paths.extend(found.inputs);
            paths.sort_unstable();
            paths.dedup();
        }
        let mut inputs = Vec::new();
        for path in &paths {
            let full = dir.join(path);
            // an input which is not a file is not sent, which an action which reads it fails without
            let Some(metadata) = std::fs::metadata(&full).ok().filter(std::fs::Metadata::is_file) else {
                continue;
            };
            let blob = crate::blake3::hash_file(&full).map_err(|_| Sent::NotRun)?;
            let executable = metadata.permissions().mode() & 0o111 != 0;
            inputs.push(crate::worker::Input { path: path.clone(), blob, executable });
        }
        let request = crate::worker::Request {
            identity: self.identity.clone(),
            release: self.release.clone(),
            install: self.install.clone(),
            target: self.target.clone(),
            action: self.action.clone(),
            env: self.env.clone(),
            locating: self.locating.clone(),
            inputs,
            outputs: self.outputs.iter().chain(&self.stderr).cloned().collect(),
        };
        let (executed, ran) = self.worker.execute_timed(dir, &request).map_err(|_| Sent::NotRun)?;
        if executed.succeeded {
            self.worker.timed(ran, self.took);
        }
        let written = |path: &String| executed.entry.outputs.iter().any(|output| output.path == *path);
        if !executed.succeeded || !self.outputs.iter().all(written) {
            return Err(Sent::Failed);
        }
        let blobs = cache::entry_blobs(&executed.entry);
        let missing: Vec<Hash> = blobs.filter(|blob| !self.cache.has_blob(blob)).copied().collect();
        let fetching = Instant::now();
        let fetched = self.worker.fetch_blobs(&self.cache, &missing);
        let size = |blob: &Hash| std::fs::metadata(self.cache.blob_path(blob)).map_or(0, |metadata| metadata.len());
        self.worker.measured(fetched.iter().map(size).sum(), fetching.elapsed());
        Restore { cache: &self.cache, entry: executed.entry }.apply(dir).map_err(|_| Sent::NotRun)
    }
    /// Returns what marks the edge to run here, once it failed on the worker and succeeds here, as it depends on what
    /// it does not declare or discover, until its first key changes.
    fn mark(&self) -> Option<(Cache, Hash)> {
        Some((self.cache.clone(), self.key?))
    }
}

/// Where the result of a cached edge is looked for: the cache, the edge's first key, and the variables every command
/// is given, by which the variables it discovered are stamped.
///
/// It is looked for on the thread which runs the edge, rather than the build's, so that edges which wait on a remote
/// cache wait together, and stamps the inputs the edge discovered itself, by their contents.
struct Lookup {
    cache: Cache,
    key: Hash,
    env: Arc<Vec<(String, String)>>,
}
impl Lookup {
    /// Returns the second key of a set of inputs the edge discovered, by their contents as they are now, relative to
    /// the build's directory, or that they are missing, and the values of the variables, as a command sees them.
    fn second_key(&self, dir: &Path, discovery: &cache::Discovery) -> Hash {
        use std::os::unix::ffi::OsStrExt;
        let hashes: Vec<_> = discovery.inputs.iter().map(|path| crate::blake3::hash_file(&dir.join(path)).ok()).collect();
        let inputs: Vec<_> = discovery.inputs.iter().zip(&hashes).map(|(path, hash)| (path.as_str(), hash.as_ref())).collect();
        let value = |name: &str| match self.env.iter().rev().find(|(variable, _)| variable == name) {
            Some((_, value)) => Some(crate::blake3::hash(value.as_bytes())),
            None => std::env::var_os(name).map(|value| crate::blake3::hash(value.as_bytes())),
        };
        let values: Vec<_> = discovery.env.iter().map(|name| value(name)).collect();
        let env: Vec<_> = discovery.env.iter().zip(&values).map(|(name, hash)| (name.as_str(), hash.as_ref())).collect();
        cache::second_key(&self.key, &inputs, &env)
    }
    /// Find the edge's result, and restore it, returning what its command printed, or `None` if it is not found or
    /// cannot be restored.
    fn restore(&self, dir: &Path) -> Option<Vec<u8>> {
        let (_, entry) = self.cache.find(&self.key, |discovery| self.second_key(dir, discovery))?;
        Restore { cache: &self.cache, entry }.apply(dir).ok()
    }
}

/// The result of an edge in the cache, which is restored to its outputs.
struct Restore<'a> {
    cache: &'a Cache,
    entry: cache::Entry,
}
impl Restore<'_> {
    /// Restore the outputs of the edge, relative to the build's directory, returning what its command printed, once
    /// the blobs which are not in the cache are fetched from the remote cache together.
    ///
    /// If one cannot be, every blob of the result is checked, so that those which are corrupted are removed, and are
    /// added again once the edge has run.
    fn apply(&self, dir: &Path) -> std::io::Result<Vec<u8>> {
        self.cache.fetch_blobs(&cache::entry_blobs(&self.entry).copied().collect::<Vec<_>>());
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
    /// Whether it ran on a worker.
    remote: bool,
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
        env: Arc::new(options.env.clone()),
        relocation: std::path::absolute(&options.dir).map(|dir| dir.display().to_string()).unwrap_or_default(),
        local: 0,
        on_worker: vec![0; options.workers.len()],
        sent: HashMap::new(),
        spawned: vec![false; options.workers.len()],
        local_only: HashSet::new(),
        stop: Arc::default(),
        returned: std::collections::VecDeque::new(),
        early_deps: HashSet::new(),
        reported: HashMap::new(),
        released: HashSet::new(),
        started: HashMap::new(),
        on_workers: HashSet::new(),
    };
    if options.jobserver && !run.edges.is_empty() {
        run.jobserver = Jobserver::from_env().or_else(|| Jobserver::create(options.jobs.max(1)).ok());
    }
    // which dependents need only the early outputs of the edges they depend on
    for (dependency, dependents) in run.dependents.iter().enumerate() {
        if run.graph.edge(run.edges[dependency]).early().is_empty() {
            continue;
        }
        for &dependent in dependents {
            if early_only(run.graph, run.state, run.edges[dependent], run.edges[dependency]) {
                run.early_deps.insert((dependent, dependency));
            }
        }
    }
    run.status = vec![Status::Waiting; run.edges.len()];
    run.progress.total = run.edges.len();
    let ready: Vec<usize> = (0..run.edges.len()).filter(|&index| run.waiting[index] == 0).collect();
    for index in ready {
        run.make_ready(index);
    }
    let ran = run.run(observer);
    // what was added to the cache is pushed to the remote cache before the build ends
    if let Some(cache) = &options.cache {
        cache.wait_for_pushes();
    }
    ran?;
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

/// Whether an edge needs only the early outputs of another of the edges it depends on: each of its inputs, declared
/// and discovered, which the other produces is one of them, and none is the output of an alias, which may lead to the
/// others.
fn early_only(graph: &Graph, state: &State, dependent: EdgeId, dependency: EdgeId) -> bool {
    let early = graph.edge(dependency).early();
    let data = graph.edge(dependent);
    let declared = data.inputs().iter().chain(data.implicit()).chain(data.order_only());
    let mut nodes: Vec<NodeId> = declared.copied().collect();
    nodes.extend(plan::discovered_nodes(graph, state, dependent));
    let mut needs = false;
    for node in nodes {
        match graph.producer(node) {
            Some(producer) if producer == dependency => {
                if !early.contains(&node) {
                    return false;
                }
                needs = true;
            },
            Some(producer) if *graph.edge(producer).kind() == EdgeKind::Alias => return false,
            _ => (),
        }
    }
    needs
}

/// Whether two paths relative to the build's directory are of the same file, with or without `./` before them.
fn same_path(lhs: &str, rhs: &str) -> bool {
    let trim = |path: &str| path.trim_start_matches("./").to_string();
    trim(lhs) == trim(rhs)
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
    /// The variables every command is given, for the threads which look edges up in the cache.
    env: Arc<Vec<(String, String)>>,
    /// The build's directory, absolute, which is replaced in the actions of cached edges by a marker in their keys, so
    /// that a build in another directory shares their results, or empty if it is not known.
    relocation: String,
    /// The number of running edges which run here, rather than on a worker.
    local: usize,
    /// The number of running edges sent to each worker, and the worker each was sent to.
    on_worker: Vec<usize>,
    sent: HashMap<usize, usize>,
    /// Whether the threads for each worker's jobs have been started.
    spawned: Vec<bool>,
    /// The edges which cannot run on a worker.
    local_only: HashSet<usize>,
    /// Set once the build is interrupted, so that an edge which a worker was interrupted running does not run here.
    stop: Arc<AtomicBool>,
    /// The edges which workers gave back, which run here once one of the build's jobs is free, in the order given.
    returned: std::collections::VecDeque<Job>,
    /// The dependents, and the edges they depend on, which need only the early outputs of those edges.
    early_deps: HashSet<(usize, usize)>,
    /// The early outputs each running edge has reported.
    reported: HashMap<usize, HashSet<NodeId>>,
    /// The dependents, and the edges they depend on, which no longer wait for them, as they reported their early
    /// outputs.
    released: HashSet<(usize, usize)>,
    /// When each running edge started.
    started: HashMap<usize, Instant>,
    /// The files the workers have from this build, as they wrote or were sent them, which are not sent again.
    on_workers: HashSet<String>,
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
        let (work, dir, env, makeflags) = (&work, &dir, &env, makeflags.as_deref());
        // a thread which takes jobs, of which there is one for each of the build's jobs, and of each worker's, once it
        // is first sent an edge
        let stop = Arc::clone(&self.stop);
        let stop = &stop;
        let job_thread = || {
            let sender = sender.clone();
            move || {
                // the lock is held while waiting for a job, which the other threads wait for in turn
                while let Ok(Ok(job)) = work.lock().map(|work| work.recv()) {
                    let Job { index, action, console, lookup, inherit, dispatch, mark, early } = job;
                    let start = SystemTime::now();
                    let clock = Instant::now();
                    // a result which cannot be restored, as the cache is corrupted, is a miss, and the action runs
                    let restored = lookup.and_then(|lookup| lookup.restore(dir));
                    let cached = restored.is_some();
                    let sent = match &dispatch {
                        Some(dispatch) if !cached => dispatch.run(dir),
                        _ => Sent::NotRun,
                    };
                    let (ran, failed_there) = match sent {
                        // what a worker printed is what the edge wrote, which is rendered as it is here
                        Sent::Ran(printed) if early.is_some() => {
                            (Some(crate::json::rendered(&printed, &mut |_| ())), false)
                        },
                        Sent::Ran(printed) => (Some(printed), false),
                        Sent::Failed => (None, true),
                        Sent::NotRun => (None, false),
                    };
                    let remote = ran.is_some();
                    // an edge which the worker did not run is given back to run in one of the build's own jobs, unless
                    // the worker was interrupted running it
                    let given_back = !cached && !remote && !stop.load(Ordering::SeqCst);
                    if let Some(dispatch) = dispatch.as_ref().filter(|_| given_back) {
                        let mark = dispatch.mark().filter(|_| failed_there);
                        let job = Job { index, action, console, lookup: None, inherit, dispatch: None, mark, early };
                        let _ = sender.send(Message::Returned(Box::new(job)));
                        continue;
                    }
                    let (outcome, output) = if let Some(printed) = restored.or(ran) {
                        (Outcome::Succeeded, printed)
                    } else if dispatch.is_some() {
                        // an edge which a worker was interrupted running does not run here
                        (Outcome::Interrupted, Vec::new())
                    } else {
                        let spawned = |pid| {
                            let _ = sender.send(Message::Spawned(index, pid));
                        };
                        // each early output which is reported is told, as it is
                        let reported = |path: &str| {
                            let outputs = early.iter().flat_map(|early| early.iter());
                            let reported = outputs.filter(|(output, _)| same_path(output, path));
                            for &(_, node) in reported {
                                let _ = sender.send(Message::Early(index, node));
                            }
                        };
                        let reports: Option<&dyn Fn(&str)> = early.as_ref().map(|_| &reported as &dyn Fn(&str));
                        let environment = Environment { makeflags, env, inherit: inherit.as_deref(), reports };
                        run_command(&action, dir, console, &environment, &spawned)
                    };
                    if outcome.succeeded() && let Some((cache, key)) = &mark {
                        cache.mark_local(key);
                    }
                    let duration = clock.elapsed();
                    let finished = Finished { index, outcome, output, start, duration, cached, remote };
                    let _ = sender.send(Message::Finished(finished));
                }
            }
        };
        std::thread::scope(|scope| {
            for _ in 0..self.options.jobs.max(1) {
                scope.spawn(job_thread());
            }
            // the workers are connected to at once, if an edge may run on one
            if (0..self.edges.len()).any(|index| self.remote_identity(index).is_some()) {
                for worker in &self.options.workers {
                    scope.spawn(|| worker.capabilities());
                }
            }
            loop {
                let cancelled = self.options.cancel.load(Ordering::SeqCst).then_some(sys::SIGINT);
                let interrupted = interrupts.interrupted().or(cancelled);
                if let Some(signal) = interrupted && !self.report.interrupted {
                    self.report.interrupted = true;
                    self.stop.store(true, Ordering::SeqCst);
                    interrupted_at = Some(Instant::now());
                    for &group in self.processes.values() {
                        sys::signal_group(group, signal);
                    }
                    for (worker, &running) in self.options.workers.iter().zip(&self.on_worker) {
                        if running > 0 {
                            worker.cancel();
                        }
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
                // what a worker gave back takes the build's jobs first, as it has started, even once the build stops,
                // unless it was interrupted
                if self.report.interrupted {
                    for job in std::mem::take(&mut self.returned) {
                        let now = SystemTime::now();
                        let outcome = Outcome::Interrupted;
                        let finished = Finished { index: job.index, outcome, output: Vec::new(), start: now,
                            duration: Duration::ZERO, cached: false, remote: false };
                        self.finish(finished, observer);
                    }
                }
                while self.local < self.options.jobs.max(1) && let Some(job) = self.returned.pop_front() {
                    match &self.jobserver {
                        Some(jobserver) if self.implicit.is_some() => {
                            let Some(token) = jobserver.try_acquire() else {
                                self.returned.push_front(job);
                                waiting_for_token = true;
                                break;
                            };
                            self.tokens.insert(job.index, token);
                        },
                        _ => self.implicit = Some(job.index),
                    }
                    self.local += 1;
                    let _ = jobs.send(job);
                }
                while !self.stopping() {
                    // an edge runs here while the build's jobs are not full, and otherwise on a worker, if one can,
                    // while one which waits for a worker only runs on one
                    let here = (self.local < self.options.jobs.max(1)).then(|| self.next_ready()).flatten();
                    let next = here.map(|index| (index, None));
                    let Some((index, worker)) = next.or_else(|| {
                        self.next_for_worker().map(|(index, worker)| (index, Some(worker)))
                    }) else {
                        break;
                    };
                    if self.restat_clean(index) {
                        self.report.restat_skipped += 1;
                        self.progress.total -= 1;
                        self.done(index, false);
                        continue;
                    }
                    // an edge runs in the job the build holds without a token if it is free, and any other takes a
                    // token, but for one which runs on a worker
                    match &self.jobserver {
                        _ if worker.is_some() => (),
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
                    let lookup = self.lookup(index);
                    let edge = self.graph.edge(self.edges[index]);
                    let inherit = edge.hermetic().then(|| {
                        let mut inherit = self.options.locating.clone();
                        inherit.extend(edge.pass_env().iter().cloned());
                        inherit
                    });
                    let dispatch = worker.and_then(|worker| self.dispatch(index, worker));
                    match worker {
                        Some(worker) => {
                            if !self.spawned[worker] {
                                self.spawned[worker] = true;
                                for _ in 0..self.options.workers[worker].jobs() {
                                    scope.spawn(job_thread());
                                }
                            }
                            self.on_worker[worker] += 1;
                            self.sent.insert(index, worker);
                        },
                        None => self.local += 1,
                    }
                    // a thread for each job takes it, as no more run than there are jobs
                    let early = self.early_outputs(index);
                    let _ = jobs.send(Job { index, action, console, lookup, inherit, dispatch, mark: None, early });
                }
                // the build goes on while an edge waits for a worker which is connecting
                if self.progress.running == 0 && !self.waiting_for_workers() {
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
                    Ok(Message::Early(index, node)) => self.early(index, node),
                    Ok(Message::Returned(job)) => {
                        let job = *job;
                        // the worker's job is free, and the edge is not sent to one again
                        if let Some(worker) = self.sent.remove(&job.index) {
                            self.on_worker[worker] -= 1;
                        }
                        self.local_only.insert(job.index);
                        self.returned.push_back(job);
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
                        match self.sent.remove(&finished.index) {
                            Some(worker) => self.on_worker[worker] -= 1,
                            None => self.local -= 1,
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
    ///
    /// An edge which waits for a worker is left for one.
    fn next_ready(&mut self) -> Option<usize> {
        let entry = *self.ready.iter().find(|&&(_, index)| {
            let console = self.console && self.graph.edge(self.edges[index]).pool() == Pool::Console;
            !console && !self.waits_for_workers(index)
        })?;
        self.ready.remove(&entry);
        Some(entry.1)
    }
    /// Whether an edge waits for a worker rather than running here, as the build is only for workers: one which may
    /// run on a worker, while one is connecting, or has reported that it runs the edge.
    fn waits_for_workers(&self, index: usize) -> bool {
        if !self.options.workers_only || self.local_only.contains(&index) {
            return false;
        }
        (0..self.options.workers.len()).any(|worker| {
            let remote = &self.options.workers[worker];
            remote.usable() && (remote.connected_jobs() == 0 || remote.reported().is_none() || self.runs(index, worker))
        }) && self.remote_identity(index).is_some()
    }
    /// Whether an edge which is ready waits for a worker.
    fn waiting_for_workers(&self) -> bool {
        self.options.workers_only && self.ready.iter().any(|&(_, index)| self.waits_for_workers(index))
    }
    /// Whether a worker may run an edge, by what it reported of itself, and has not refused its toolchain.
    fn runs(&self, index: usize, worker: usize) -> bool {
        let edge = self.graph.edge(self.edges[index]);
        let Some(identity) = edge.cache() else {
            return false;
        };
        let placement = edge.placement();
        let release = self.options.releases.get(identity).cloned().unwrap_or_default();
        let needs = crate::worker::Needs {
            identity,
            release: if placement.any_host { &release.identity } else { "" },
            install: &release.install,
            target: placement.target.as_deref().unwrap_or_default(),
            tags: &placement.tags,
        };
        let remote = &self.options.workers[worker];
        remote.may_have(identity) && remote.reported().is_some_and(|capabilities| capabilities.may_run(&needs))
    }
    /// Returns the edge of the highest priority which is ready to run and may run on a worker, with a worker which
    /// has a free job, and may run it, by what it reported of itself, if there is one.
    fn next_for_worker(&mut self) -> Option<(usize, usize)> {
        // a worker is connected to on a thread of its own, so that the build does not wait for it
        let free = |run: &Self, worker: usize| run.on_worker[worker] < run.options.workers[worker].connected_jobs();
        if !(0..self.options.workers.len()).any(|worker| free(self, worker)) {
            return None;
        }
        let ready: Vec<(std::cmp::Reverse<u64>, usize)> = self.ready.iter().copied().collect();
        for entry in ready {
            let index = entry.1;
            if self.local_only.contains(&index) {
                continue;
            }
            if self.remote_identity(index).is_none() {
                self.local_only.insert(index);
                continue;
            }
            // the worker it would finish soonest on, by sending it, and running it there
            let took = self.took(index);
            let there = |worker: usize| {
                let link = self.options.workers[worker].link();
                self.sending(index, link).saturating_add(link.running(took))
            };
            let candidates = (0..self.options.workers.len())
                .filter(|&worker| free(self, worker) && self.runs(index, worker));
            let worker = candidates.min_by_key(|&worker| there(worker));
            // it waits for a job here if it would finish sooner here, unless it only runs on workers
            let wait = self.local_wait();
            let worker = worker.filter(|&worker| {
                self.options.workers_only || wait.is_none_or(|wait| there(worker) < wait.saturating_add(took))
            });
            if let Some(worker) = worker {
                self.ready.remove(&entry);
                return Some((index, worker));
            }
        }
        None
    }
    /// Returns how long it is estimated an edge would wait for one of the build's own jobs: the least time one of the
    /// edges which run here has left, by how long it took when it last ran, or `None` if none of them has run before.
    fn local_wait(&self) -> Option<Duration> {
        let running = self.started.iter().filter(|(index, _)| !self.sent.contains_key(index));
        let left = running.filter_map(|(&index, started)| {
            let record = self.state.get(&plan::first_output(self.graph, self.edges[index]))?;
            Some(Duration::from_millis(u64::from(record.duration)).saturating_sub(started.elapsed()))
        });
        left.min()
    }
    /// Returns how long an edge took when it last ran, or nothing if it has not run.
    fn took(&self, index: usize) -> Duration {
        let record = self.state.get(&plan::first_output(self.graph, self.edges[index]));
        record.map_or(Duration::ZERO, |record| Duration::from_millis(u64::from(record.duration)))
    }
    /// Returns how long it is estimated to take to send an edge to a worker over a link, and fetch its outputs: its
    /// files' sizes when it last ran, but for those the worker has, in the round trips of asking which blobs the
    /// worker is missing, sending them, running the edge, and fetching its outputs.
    fn sending(&self, index: usize, link: crate::remote::Link) -> Duration {
        let Some(previous) = self.state.get(&plan::first_output(self.graph, self.edges[index])) else {
            return Duration::MAX;
        };
        let files = previous.inputs.iter().chain(&previous.discovered).chain(&previous.outputs);
        let bytes = files.filter(|(path, _)| !self.on_workers.contains(path)).map(|(_, stamp)| stamp.size()).sum();
        link.time(bytes, 4)
    }
    /// Returns the identity of the toolchain of an edge which may run on a worker: a cached edge which is not in the
    /// console pool, which inherits only the variables which locate its tools, and which has run before, so that the
    /// inputs it discovered are known, whose paths all lead from the build's directory, as its outputs' do.
    fn remote_identity(&self, index: usize) -> Option<&str> {
        let edge = self.graph.edge(self.edges[index]);
        let identity = edge.cache()?;
        if edge.pool() == Pool::Console || !edge.hermetic() || self.options.cache.is_none() {
            return None;
        }
        let outputs = edge.outputs().iter().chain(edge.implicit_outputs()).map(|&node| self.graph.path(node));
        let outputs: Vec<&str> = outputs.chain(edge.depfile()).collect();
        let leads = |path: &str| crate::worker::ups(path).is_some();
        // an edge which has not run finds its inputs by its action which discovers them, if it has one
        let Some(previous) = self.state.get(&plan::first_output(self.graph, self.edges[index])) else {
            let declared = edge.inputs().iter().chain(edge.implicit()).map(|&node| self.graph.path(node));
            let discovers = edge.discover().is_some() && edge.depfile().is_some();
            return (discovers && declared.chain(outputs).all(leads)).then_some(identity);
        };
        let recorded = previous.inputs.iter().chain(&previous.discovered).map(|(path, _)| path.as_str());
        recorded.chain(outputs).all(leads).then_some(identity)
    }
    /// Returns how a started edge runs on a worker: with the inputs it declares now, and those it discovered when it
    /// last ran, or `None` if one does not lead from the build's directory, or it is marked to run here, when it runs
    /// here.
    fn dispatch(&mut self, index: usize, worker: usize) -> Option<Dispatch> {
        let edge = self.graph.edge(self.edges[index]);
        let cache = self.options.cache.as_ref()?.without_remote();
        let key = self.keys.get(&index).copied();
        if key.is_some_and(|key| cache.is_local(&key)) {
            self.local_only.insert(index);
            return None;
        }
        let previous = self.state.get(&plan::first_output(self.graph, self.edges[index]));
        // one which has not run discovers its inputs first, as it is sent
        let discovery = match previous {
            Some(_) => None,
            None => Some((edge.discover()?.clone(), edge.depfile()?.to_string())),
        };
        let declared = self.inputs.get(&index)?.iter().map(|(path, _)| path);
        let discovered = previous.into_iter().flat_map(|previous| previous.discovered.iter().map(|(path, _)| path));
        let mut inputs: Vec<String> = declared.chain(discovered).cloned().collect();
        inputs.sort_unstable();
        inputs.dedup();
        let outputs = edge.outputs().iter().chain(edge.implicit_outputs()).map(|&node| self.graph.path(node));
        let outputs: Vec<String> = outputs.chain(edge.depfile()).map(String::from).collect();
        let stderr = match self.action(index) {
            Action::Exec(exec) => exec.stderr.clone(),
            Action::Shell(_) => None,
        };
        if !inputs.iter().chain(&outputs).chain(&stderr).all(|path| crate::worker::ups(path).is_some()) {
            self.local_only.insert(index);
            return None;
        }
        // what it is sent, the worker has
        self.on_workers.extend(inputs.iter().cloned());
        let mut env = self.options.env.clone();
        for name in edge.pass_env() {
            if let Some(value) = std::env::var_os(name) {
                env.push((name.clone(), value.to_string_lossy().into_owned()));
            }
        }
        let identity = edge.cache()?;
        let release = self.options.releases.get(identity).cloned().unwrap_or_default();
        let placement = edge.placement();
        Some(Dispatch {
            worker: Arc::clone(&self.options.workers[worker]),
            cache,
            key,
            took: self.took(index),
            identity: identity.to_string(),
            release: if placement.any_host { release.identity } else { String::new() },
            install: release.install,
            target: placement.target.clone().unwrap_or_default(),
            action: plan::relocated(self.action(index), &self.relocation),
            env,
            locating: self.options.locating.clone(),
            inputs,
            outputs,
            stderr,
            discovery,
        })
    }
    /// Returns the early outputs of an edge, by their paths, if it has any.
    fn early_outputs(&self, index: usize) -> Option<Arc<Vec<(String, NodeId)>>> {
        let early = self.graph.edge(self.edges[index]).early();
        let paths = || early.iter().map(|&node| (self.graph.path(node).to_string(), node)).collect();
        (!early.is_empty()).then(|| Arc::new(paths()))
    }
    /// An edge has written an early output, which is stamped afresh, and once it has written each, the edges which
    /// need only them are no longer waiting for it.
    fn early(&mut self, index: usize, node: NodeId) {
        self.stamps.forget(node);
        let reported = self.reported.entry(index).or_default();
        reported.insert(node);
        if reported.len() < self.graph.edge(self.edges[index]).early().len() {
            return;
        }
        for dependent in self.dependents[index].clone() {
            if self.early_deps.contains(&(dependent, index)) && self.released.insert((dependent, index)) {
                self.waiting[dependent] -= 1;
                if self.waiting[dependent] == 0 {
                    self.make_ready(dependent);
                }
            }
        }
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
        // what started before an edge it depends on finished does not know whether its outputs changed
        if self.deps[index].iter().any(|&dep| !matches!(self.status[dep], Status::Done { .. })) {
            return false;
        }
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
        self.started.insert(index, Instant::now());
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
        let Finished { index, mut outcome, output, start, mut duration, cached, remote } = finished;
        let edge = self.edges[index];
        let data = self.graph.edge(edge);
        self.started.remove(&index);
        // what a worker wrote, it has
        if remote {
            let outputs = data.outputs().iter().chain(data.implicit_outputs()).map(|&node| self.graph.path(node));
            self.on_workers.extend(outputs.chain(data.depfile()).map(String::from));
        }
        self.progress.running -= 1;
        self.progress.finished += 1;
        self.report.ran += 1;
        let inputs = self.inputs.remove(&index).unwrap_or_default();
        let key = self.keys.remove(&index);
        if remote {
            self.report.remote += 1;
        }
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
    /// Returns where the result of a cached edge is looked for, by its first key, which its result is added to the
    /// cache by if it runs. The thread which runs it looks for it, by the sets of inputs the edge discovered with that
    /// key.
    fn lookup(&mut self, index: usize) -> Option<Lookup> {
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
        Some(Lookup { cache: cache.clone(), key, env: Arc::clone(&self.env) })
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
            // one which started once the edge's early outputs were written does not wait for it
            if self.released.contains(&(dependent, index)) {
                continue;
            }
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
pub(crate) struct Environment<'a> {
    /// The value of `MAKEFLAGS` which gives it the jobserver, if rig created one.
    pub(crate) makeflags: Option<&'a str>,
    pub(crate) env: &'a [(String, String)],
    /// The variables rig is run with which a hermetic command inherits, or `None` if it inherits every one.
    pub(crate) inherit: Option<&'a [String]>,
    /// For a command which writes what rustc writes with JSON, what is told the path of each artifact it reports, as it
    /// is reported, while what it would have printed is kept.
    pub(crate) reports: Option<&'a dyn Fn(&str)>,
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
pub(crate) fn run_command(
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
    let read = match environment.reports {
        None => reader.read_to_end(&mut output),
        // each line is read as it is written, so that an artifact is told as soon as it is reported
        Some(reports) => {
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break Ok(0),
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        match crate::json::message(text.trim_end_matches(['\n', '\r'])) {
                            crate::json::Message::Artifact(path) => reports(&path),
                            crate::json::Message::Printed(printed) => output.extend_from_slice(printed.as_bytes()),
                        }
                    },
                    Err(error) => break Err(error),
                }
            }
        },
    };
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
///
/// Without `colour`, as where the output is not a terminal, the escape sequences which colour what the commands
/// printed are left out, as ninja leaves them out, so that commands can be told to print in colour, as rustc is with
/// `--color=always`, whose output is captured rather than a terminal, without their actions depending on where the
/// build's output goes.
pub struct Console<W: std::io::Write> {
    pub out: W,
    pub quiet: bool,
    pub verbose: bool,
    pub colour: bool,
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
        let plain;
        let output = if self.colour {
            output
        } else {
            plain = without_colour(output);
            &plain
        };
        let _ = self.out.write_all(output);
        if !output.is_empty() && !output.ends_with(b"\n") {
            let _ = writeln!(self.out);
        }
        let _ = self.out.flush();
    }
}

/// Returns what a command printed without the escape sequences which colour it, `ESC [ ... <letter>`, and those which
/// choose the character set, `ESC ( <byte>`, as rustc writes to reset it.
#[must_use]
pub fn without_colour(text: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(text.len());
    let mut bytes = text.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != 0x1b {
            plain.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'[') => {
                for byte in bytes.by_ref() {
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
            },
            Some(b'(') => _ = bytes.next(),
            _ => (),
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Exec;

    #[test]
    fn removes_colour() {
        let coloured = b"\x1b[0m\x1b[1m\x1b[33mwarning\x1b[0m\x1b[0m\x1b[1m: unneeded `return`\x1b(B\x1b[m\n";
        assert_eq!(without_colour(coloured), b"warning: unneeded `return`\n");
        assert_eq!(without_colour("plain \u{e9}\n".as_bytes()), "plain \u{e9}\n".as_bytes());
        // and from what a console shows without colour
        let graph = graph().rule("say", &[("command", "printf '\\033[1mbold\\033[0m\\n' && touch $out")]);
        let mut build = Build::new("colour", graph.edge("say", &["a"], &[]).finish(), &[]);
        let mut console = Console { out: Vec::new(), quiet: true, verbose: false, colour: false };
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        assert_eq!(String::from_utf8(console.out).unwrap(), "bold\n");
    }
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
        assert_eq!(build.build(&[]).1, Vec::<(String, String)>::new());
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
        let mut console = Console { out: Vec::new(), quiet: true, verbose: false, colour: true };
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

    /// A graph of two cached edges which each discover `extra.h`, and print, of one which reads `hidden`, which it
    /// does not declare or discover, and of the slow edge, which is not cached.
    fn compiled() -> Graph {
        let cc = "cat $in extra.h > $out && echo \"$out: $in extra.h\" > $out.d && echo compiled $out";
        graph()
            .rule("slow", &[("command", SLOW)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d")])
            .rule("hidden", &[
                ("command", "cat $in hidden > $out && echo \"$out: $in\" > $out.d"),
                ("depfile", "$out.d"),
            ])
            .edge("cc", &["a.o"], &["a.c"])
            .edge("cc", &["b.o"], &["b.c"])
            .edge("hidden", &["c.o"], &["c.c"])
            .finish()
    }

    /// A worker whose store is a cache in a directory of its own, which has toolchains by these identities.
    fn worker(cache: &TempCache, identities: &[&str]) -> Arc<Remote> {
        let toolchain =
            |identity: &&str| crate::worker::Toolchain { identity: (*identity).into(), ..Default::default() };
        let toolchains = identities.iter().map(toolchain).collect();
        let capabilities = crate::worker::Capabilities { toolchains, ..Default::default() };
        let executor = crate::worker::Executor::new(cache.0.dir().join("work"), 3, move || capabilities.clone());
        Arc::new(crate::remote::served_worker(cache.0.clone(), executor))
    }

    #[test]
    fn runs_edges_on_workers_once_its_jobs_are_full() {
        let files = [("a.c", "a\n"), ("b.c", "b\n"), ("c.c", "c\n"), ("extra.h", "h\n"), ("hidden", "hidden\n")];
        let mut build = Build::new("workers", compiled(), &files);
        // the first build runs every edge here, as the inputs they discover are not known yet
        let first = TempCache::new("workers-first");
        build.options.cache = Some(first.0.clone());
        build.options.jobs = 1;
        let store = TempCache::new("workers-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        let (report, finished) = build.build(&[]);
        assert_eq!((report.ran, report.remote), (4, 0), "{finished:?}");
        // once they are, with an empty cache, the slow edge holds the one job, which the others would wait for
        // longer than they take to send, so the worker runs them, but for the one which reads what it was not sent,
        // which fails there and runs here again
        remove(&build, &["slow", "a.o", "b.o", "c.o", "a.o.d", "b.o.d"]);
        let second = TempCache::new("workers-second");
        build.options.cache = Some(second.0.clone());
        // which is connected to first, and reports what it has, as the build does not wait for it
        assert_eq!(build.options.workers[0].jobs(), 3);
        assert!(build.options.workers[0].capabilities().is_ok());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.cached, report.remote), (4, 0, 2), "{finished:?}");
        assert_eq!(build.read("a.o"), "a\nh\n");
        assert_eq!(build.read("b.o"), "b\nh\n");
        assert_eq!(build.read("c.o"), "c\nhidden\n");
        assert_eq!(build.read("a.o.d"), "a.o: a.c extra.h\n");
        let mut printed: Vec<String> = finished.iter().map(|(_, printed)| printed.clone()).collect();
        printed.sort();
        assert_eq!(printed, ["", "", "compiled a.o\n", "compiled b.o\n"]);
        // what ran on the worker is recorded, and added to the cache, as what ran here is
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        assert_eq!(second.0.stats().results, 3);
        // a worker without the toolchain is sent nothing
        remove(&build, &["slow", "a.o", "b.o", "c.o"]);
        let third = TempCache::new("workers-third");
        build.options.cache = Some(third.0.clone());
        let other = TempCache::new("workers-other");
        build.options.workers = vec![worker(&other, &["another toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        let (report, finished) = build.build(&[]);
        assert_eq!((report.ran, report.remote), (4, 0), "{finished:?}");
        assert_eq!(other.0.stats().blobs, 0);
        assert_eq!(build.read("b.o"), "b\nh\n");
    }

    /// The rule of an edge which is not cached, and takes long enough that it is the first to run, by how long it took
    /// when it last ran, and holds the one job a build has, so that the others run on a worker, if they can.
    const SLOW: &str = "sleep 0.3 && touch $out";

    /// Remove outputs, so that their edges run again.
    fn remove(build: &Build, outputs: &[&str]) {
        for output in outputs {
            std::fs::remove_file(build.dir.join(output)).unwrap();
        }
    }

    #[test]
    fn sends_edges_to_workers_which_can_run_them() {
        use crate::worker::{Capabilities, Release, Toolchain};
        // an edge which may run on another host, an edge for a target, and an edge which needs a tag
        let any_host = crate::Placement { any_host: true, ..crate::Placement::default() };
        let riscv = crate::Placement { target: Some("riscv".into()), ..crate::Placement::default() };
        let gpu = crate::Placement { any_host: true, tags: vec!["gpu".into()], ..crate::Placement::default() };
        let cc = "cat $in > $out && echo \"$out: $in\" > $out.d";
        let graph = graph()
            .rule("slow", &[("command", SLOW)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d")])
            .placed(any_host)
            .edge("cc", &["portable.o"], &["portable.c"])
            .placed(riscv)
            .edge("cc", &["riscv.o"], &["riscv.c"])
            .placed(gpu)
            .edge("cc", &["gpu.o"], &["gpu.c"])
            .finish();
        let files = [("portable.c", "p\n"), ("riscv.c", "r\n"), ("gpu.c", "g\n")];
        let mut build = Build::new("placed", graph, &files);
        let first = TempCache::new("placed-first");
        build.options.cache = Some(first.0.clone());
        let release = Release { identity: "release".into(), install: String::new() };
        build.options.releases.insert("toolchain".into(), release);
        build.ran(&[]);
        // a worker of another toolchain of the same release, as on another host, with no tags, and no standard
        // library for the target, so that only the edge which may run on another host, and needs no tag, runs there
        let (identity, release_identity) = ("elsewhere".into(), "release".into());
        let elsewhere = Toolchain { identity, release_identity, ..Toolchain::default() };
        let store = TempCache::new("placed-store");
        let capabilities = Capabilities { toolchains: vec![elsewhere], ..Capabilities::default() };
        let executor = crate::worker::Executor::new(store.0.dir().join("work"), 4, move || capabilities.clone());
        let worker = Arc::new(crate::remote::served_worker(store.0.clone(), executor));
        assert!(worker.capabilities().is_ok());
        build.options.workers = vec![worker];
        build.options.jobs = 1;
        remove(&build, &["slow", "portable.o", "riscv.o", "gpu.o"]);
        let second = TempCache::new("placed-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 1, "{finished:?}");
        assert_eq!(build.read("portable.o"), "p\n");
        let sent = |contents: &[u8]| store.0.has_blob(&crate::blake3::hash(contents));
        assert!(sent(b"p\n") && !sent(b"r\n") && !sent(b"g\n"));
    }

    /// A graph of the slow edge, and of two cached edges which copy their inputs, once they have slept for as many
    /// seconds as their inputs give.
    fn copied() -> Graph {
        graph()
            .rule("slow", &[("command", SLOW)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cp", &[("command", "sleep $$(cat $in) && cp $in $out")])
            .edge("cp", &["a.o"], &["a"])
            .edge("cp", &["b.o"], &["b"])
            .finish()
    }

    /// A worker over a link which is assumed to be as given.
    fn linked(cache: &TempCache, link: crate::remote::Link) -> Arc<Remote> {
        let worker = Arc::into_inner(worker(cache, &["toolchain"])).unwrap().with_link(link);
        assert!(worker.capabilities().is_ok());
        Arc::new(worker)
    }

    #[test]
    fn waits_for_its_jobs_where_sending_would_take_longer() {
        let mut build = Build::new("waits", copied(), &[("a", "0\n"), ("b", "0\n")]);
        let first = TempCache::new("waits-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        build.options.jobs = 1;
        // over a link with a second's latency, sending takes longer than the slow edge has left, or the others take
        let store = TempCache::new("waits-store");
        let slow = crate::remote::Link { latency: Duration::from_secs(1), bytes_per_second: 1e9, speed: 1.0 };
        build.options.workers = vec![linked(&store, slow)];
        remove(&build, &["slow", "a.o", "b.o"]);
        let second = TempCache::new("waits-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 0, "{finished:?}");
        assert_eq!(store.0.stats().blobs, 0);
        // while over a fast link, they are sent
        let fast = TempCache::new("waits-fast");
        let link = crate::remote::Link { latency: Duration::from_millis(1), bytes_per_second: 1e9, speed: 1.0 };
        build.options.workers = vec![linked(&fast, link)];
        remove(&build, &["slow", "a.o", "b.o"]);
        let third = TempCache::new("waits-third");
        build.options.cache = Some(third.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 2, "{finished:?}");
    }

    #[test]
    fn runs_only_on_workers_where_they_can() {
        let mut build = Build::new("workers-only", copied(), &[("a", "0\n"), ("b", "0\n")]);
        let first = TempCache::new("workers-only-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        // with jobs to spare here, the edges which a worker may run wait for one, which is still connecting as the
        // build starts, while the slow edge, which is not cached, runs here
        build.options.jobs = 8;
        build.options.workers_only = true;
        let store = TempCache::new("workers-only-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        remove(&build, &["slow", "a.o", "b.o"]);
        let second = TempCache::new("workers-only-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.remote), (3, 2), "{finished:?}");
        // what no worker runs, as they do not have its toolchain, or cannot be reached, runs here
        let other = TempCache::new("workers-only-other");
        let unreachable = Arc::new(Remote::command(vec!["/nonexistent/worker".into()], false));
        for workers in [vec![worker(&other, &["another toolchain"])], vec![unreachable]] {
            build.options.workers = workers;
            remove(&build, &["slow", "a.o", "b.o"]);
            let fresh = TempCache::new("workers-only-fresh");
            build.options.cache = Some(fresh.0.clone());
            let (report, finished) = build.build(&[]);
            assert!(report.succeeded(), "{finished:?}");
            assert_eq!((report.ran, report.remote), (3, 0), "{finished:?}");
        }
        assert_eq!(other.0.stats().blobs, 0);
    }

    #[test]
    fn sends_edges_to_the_worker_they_finish_soonest_on() {
        // the edges which copy their inputs take 0.2 s here, while the slow edge holds the one job for 0.3 s
        let mut build = Build::new("soonest", copied(), &[("a", "0.2\n"), ("b", "0.2\n")]);
        let first = TempCache::new("soonest-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        build.options.jobs = 1;
        // a worker which takes ten times as long as here would finish them later than here, and one which takes half
        // as long, sooner, so of the two, that one takes them
        let link = |speed| crate::remote::Link { latency: Duration::from_millis(1), bytes_per_second: 1e9, speed };
        let (slow, fast) = (TempCache::new("soonest-slow"), TempCache::new("soonest-fast"));
        build.options.workers = vec![linked(&slow, link(10.0)), linked(&fast, link(0.5))];
        remove(&build, &["slow", "a.o", "b.o"]);
        let second = TempCache::new("soonest-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 2, "{finished:?}");
        assert_eq!(slow.0.stats().blobs, 0);
        // and with only the slow one, they wait for the job here
        let alone = TempCache::new("soonest-alone");
        build.options.workers = vec![linked(&alone, link(10.0))];
        remove(&build, &["slow", "a.o", "b.o"]);
        let third = TempCache::new("soonest-third");
        build.options.cache = Some(third.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 0, "{finished:?}");
        assert_eq!(alone.0.stats().blobs, 0);
    }

    #[test]
    fn measures_how_fast_workers_run_edges() {
        // an edge which took 0.2 s here, which a worker runs as fast, is measured as taking as long there
        let mut build = Build::new("speed", copied(), &[("a", "0.2\n"), ("b", "0.2\n")]);
        let first = TempCache::new("speed-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        build.options.jobs = 1;
        let store = TempCache::new("speed-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        remove(&build, &["slow", "a.o", "b.o"]);
        let second = TempCache::new("speed-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded() && report.remote > 0, "{report:?} {finished:?}");
        let speed = build.options.workers[0].link().speed;
        assert!((0.5..3.0).contains(&speed), "{speed}");
    }

    #[test]
    fn sends_what_has_not_run_once_it_discovers_its_inputs() {
        // edges which read `extra.h`, which they do not declare, and which have not run, one of which finds it by
        // its rule's action which discovers its inputs, and one of which fails to
        let cc = "cat $in extra.h > $out && echo \"$out: $in extra.h\" > $out.d";
        let graph = graph()
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d"), ("discover", "echo \"$out: $in extra.h\" > $out.d")])
            .rule("lost", &[("command", cc), ("depfile", "$out.d"), ("discover", "false")])
            .edge("cc", &["a.o"], &["a.c"])
            .edge("lost", &["b.o"], &["b.c"])
            .finish();
        let mut build = Build::new("discovers", graph, &[("a.c", "a\n"), ("b.c", "b\n"), ("extra.h", "h\n")]);
        let cache = TempCache::new("discovers-cache");
        build.options.cache = Some(cache.0.clone());
        build.options.workers_only = true;
        let store = TempCache::new("discovers-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        // the first build sends the one which found what it reads with it, while the other runs here
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.remote), (2, 1), "{finished:?}");
        assert_eq!(build.read("a.o"), "a\nh\n");
        assert_eq!(build.read("a.o.d"), "a.o: a.c extra.h\n");
        assert!(store.0.has_blob(&crate::blake3::hash(b"h\n")) && !store.0.has_blob(&crate::blake3::hash(b"b\n")));
        // what it found is recorded, as if it had run here
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("extra.h", "changed\n");
        assert_eq!(build.ran(&[]).len(), 2);
    }

    #[test]
    fn starts_what_needs_early_outputs_once_they_are_written() {
        // an edge which writes its metadata, reports it as rustc does, prints a diagnostic, and goes on for a while,
        // before it writes the rest
        // as a ninja file has it, with `$` escaped, and printed with `printf`, which `sh` does not take escapes in
        let artifact = r#"{"$$message_type":"artifact","artifact":"a.meta","emit":"metadata"}"#;
        let diagnostic = r#"{"$$message_type":"diagnostic","message":"unused","rendered":"warning: unused\n"}"#;
        let report = |json: &str| format!("printf '%s\\n' '{json}' >&2");
        let compile = format!(
            "echo meta > a.meta && {} && {} && sleep 0.5 && echo all > a.out",
            report(artifact),
            report(diagnostic),
        );
        let graph = graph()
            .rule("compile", &[("command", &compile)])
            .early(&["a.meta"])
            .edge("compile", &["a.out", "|", "a.meta"], &[])
            .rule("cp", &[("command", "cp $in $out")])
            .edge("cp", &["b.out"], &["a.meta"])
            .edge("cp", &["c.out"], &["a.out"])
            .finish();
        let mut build = Build::new("early", graph, &[]);
        build.options.jobs = 4;
        let clock = Instant::now();
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        // what needs only the metadata finishes before what wrote it, while what needs the rest waits for it
        let order: Vec<&str> = finished.iter().map(|(output, _)| output.as_str()).collect();
        assert_eq!(order, ["b.out", "a.out", "c.out"], "{:?}", clock.elapsed());
        assert_eq!(build.read("b.out"), "meta\n");
        // what it would have printed is shown, rather than its JSON
        assert_eq!(finished[1].1, "warning: unused\n");
        // and once it is up to date, nothing runs
        assert_eq!(build.ran(&[]), Vec::<String>::new());
    }

    #[test]
    fn interrupts_what_runs_on_workers() {
        let mut build = Build::new("interrupts", copied(), &[("a", "0\n"), ("b", "0\n")]);
        let first = TempCache::new("interrupts-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        build.options.jobs = 1;
        let store = TempCache::new("interrupts-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        // the edges which copy their inputs sleep for ten seconds, on the worker, while the slow edge runs here
        remove(&build, &["slow"]);
        build.write("a", "10\n");
        build.write("b", "10\n");
        let cancel = Arc::clone(&build.options.cancel);
        let cancelling = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            cancel.store(true, Ordering::SeqCst);
        });
        let clock = Instant::now();
        let (report, finished) = build.build(&[]);
        cancelling.join().unwrap();
        // the worker interrupts them at once, and they do not run here then
        assert!(clock.elapsed() < Duration::from_secs(5), "{:?}", clock.elapsed());
        assert!(report.interrupted && report.remote == 0, "{report:?} {finished:?}");
        assert_eq!((build.read("a.o"), build.read("b.o")), ("0\n".into(), "0\n".into()));
        assert!(store.0.has_blob(&crate::blake3::hash(b"10\n")));
    }

    #[test]
    fn runs_what_fails_on_workers_in_its_own_jobs() {
        // the slow edge, and the edge which reads what it is not sent, take the same lock as they run here, which is
        // free only if they do not run at once, as the build has one job
        let slow = "mkdir running && sleep 0.3 && rmdir running && touch $out";
        let hidden = "mkdir running && cat $in hidden > $out && rmdir running && echo \"$out: $in\" > $out.d";
        let graph = graph()
            .rule("slow", &[("command", slow)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("hidden", &[("command", hidden), ("depfile", "$out.d")])
            .edge("hidden", &["c.o"], &["c.c"])
            .finish();
        let mut build = Build::new("own-jobs", graph, &[("c.c", "c\n"), ("hidden", "hidden\n")]);
        let first = TempCache::new("own-jobs-first");
        build.options.cache = Some(first.0.clone());
        build.options.jobs = 1;
        build.ran(&[]);
        // it fails on the worker at once, while the slow edge holds the job, which it then waits for
        let store = TempCache::new("own-jobs-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        remove(&build, &["slow", "c.o"]);
        let second = TempCache::new("own-jobs-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.remote), (2, 0), "{finished:?}");
        assert_eq!(build.read("c.o"), "c\nhidden\n");
        assert!(store.0.has_blob(&crate::blake3::hash(b"c\n")));
        // and it is marked to run here, as it failed there and succeeded here
        assert_eq!(std::fs::read_dir(second.0.dir().join("local")).map_or(0, Iterator::count), 1);
    }

    #[test]
    fn marks_edges_which_fail_on_workers_to_run_here() {
        let cc = "cat $in extra.h > $out && echo \"$out: $in extra.h\" > $out.d && echo compiled $out";
        let hidden = "cat $in hidden > $out && echo \"$out: $in\" > $out.d";
        let graph = graph()
            .rule("slow", &[("command", SLOW)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d")])
            .rule("hidden", &[("command", hidden), ("depfile", "$out.d")])
            .edge("cc", &["a.o"], &["a.c"])
            .edge("hidden", &["c.o"], &["c.c"])
            .finish();
        let files = [("a.c", "a\n"), ("c.c", "c\n"), ("extra.h", "h\n"), ("hidden", "hidden\n")];
        let mut build = Build::new("marks", graph, &files);
        let first = TempCache::new("marks-first");
        build.options.cache = Some(first.0.clone());
        build.ran(&[]);
        build.options.jobs = 1;
        // the edge which reads what it was not sent fails there, and runs here, so it is marked to run here
        let store = TempCache::new("marks-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        remove(&build, &["slow", "a.o", "c.o"]);
        let second = TempCache::new("marks-second");
        build.options.cache = Some(second.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 1, "{finished:?}");
        assert_eq!(build.read("c.o"), "c\nhidden\n");
        let marks = |cache: &TempCache| std::fs::read_dir(cache.0.dir().join("local")).map_or(0, Iterator::count);
        assert_eq!(marks(&second), 1);
        // so, with the mark, it is not sent again, while the other is, whose first key is not marked
        let third = TempCache::new("marks-third");
        std::fs::create_dir_all(third.0.dir().join("local")).unwrap();
        for mark in std::fs::read_dir(second.0.dir().join("local")).unwrap().flatten() {
            std::fs::copy(mark.path(), third.0.dir().join("local").join(mark.file_name())).unwrap();
        }
        let other = TempCache::new("marks-other");
        build.options.workers = vec![worker(&other, &["toolchain"])];
        assert!(build.options.workers[0].capabilities().is_ok());
        remove(&build, &["slow", "a.o", "c.o"]);
        build.options.cache = Some(third.0.clone());
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(report.remote, 1, "{finished:?}");
        let sent = |contents: &[u8]| other.0.has_blob(&crate::blake3::hash(contents));
        assert!(sent(b"a\n") && !sent(b"c\n"));
        // and the cache's collection keeps a mark for a month, then removes it, so that the edge is tried there again
        assert_eq!(marks(&third), 1);
        let _ = third.0.gc(u64::MAX);
        assert_eq!(marks(&third), 1);
        let month = SystemTime::now() - Duration::from_hours(24 * 31);
        for mark in std::fs::read_dir(third.0.dir().join("local")).unwrap().flatten() {
            std::fs::File::options().write(true).open(mark.path()).unwrap().set_modified(month).unwrap();
        }
        let _ = third.0.gc(u64::MAX);
        assert_eq!(marks(&third), 0);
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
        let mut console = Console { out: Vec::new(), quiet: false, verbose: false, colour: true };
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
