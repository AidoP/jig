//! Running a build: the edges a [`plan`](crate::plan::plan) finds must run are run in parallel, each once the edges it
//! depends on have run.
//!
//! # Running an edge
//!
//! Each edge runs its program, without a shell, in the build's directory. At most [`Options::jobs`] run at once, each
//! on a thread of its own. An edge's output, stdout and stderr together, is captured and given to an [`Observer`] when
//! it finishes. An edge in the console pool instead has the terminal: only one console edge runs at a time, and the
//! output of the other edges is held until it finishes. An edge in the serial pool starts once nothing else is
//! running, and nothing else starts until it has finished. An edge which succeeds is recorded at once, with the inputs
//! its depfile lists, so that an interrupted build loses only the edges which were running.
//!
//! An edge with a timeout which runs for longer is killed, with its process group, and fails with
//! [`Outcome::TimedOut`]. An edge which runs [`always`](crate::Edge::always), as a test does, runs whenever the build
//! wants it, and is given what [`Options::given`] has for it beside its action.
//!
//! A generator edge is run like any other. A build whose graph is generated should first build the file the graph is
//! read from, and read the graph again if that ran.
//!
//! # Restat
//!
//! The outputs of a restat edge are stamped again once it has run. If none changed, an edge which was only going to
//! run because it depends on them is checked again, and does not run if it is otherwise up to date.
//!
//! # The cache
//!
//! If the build has a [`Cache`], a cached edge is looked up in it before it runs, by its action and the contents of
//! its inputs. On a hit, its outputs and depfile are restored from the cache and what its command printed is shown,
//! and the command does not run. On a miss, the edge runs, and if it succeeds, its outputs, its depfile and what it
//! printed are added to the cache. A cache which cannot be read or written counts as a miss, not a failure.
//!
//! # Workers
//!
//! Once the build's own jobs are full, a cached edge may run on one of [`Options::workers`] which has the edge's
//! toolchain. Its inputs must be known, either from when it last ran or from its action which discovers them
//! ([`Edge::discover`](crate::Edge::discover)). The inputs, declared and discovered, are sent to the worker, as
//! [`crate::worker`] describes. The worker runs the edge, and its outputs are fetched and restored as if from the
//! cache. If the edge cannot be sent, or fails there, it is given back and runs here like any other edge, once a job
//! is free.
//!
//! An edge only goes to a worker if it is estimated to finish sooner there than here.
//!
//! - There: the time to send its files, run it and fetch its outputs. Sending is estimated from the size of its files
//!   when it last ran, leaving out those the worker already has from this build, and the measured latency and
//!   bandwidth of the link. Running is estimated from how long it last took and the measured speed of the worker.
//! - Here: the wait for one of the build's own jobs, which is the least time any edge running here has left, plus how
//!   long it last took.
//!
//! Of the workers which may run it, the one it would finish soonest on takes it. When a build is interrupted, its
//! workers interrupt what they are running for it, and those edges do not then run here.
//!
//! With [`Options::workers_only`], an edge which a worker may run never runs here. It waits for a worker, however many
//! jobs are free, as long as some worker is still connecting or has reported that it can run the edge. These still run
//! here: an edge no worker can run, an edge which has not run before and has no action to discover its inputs, and an
//! edge which failed on a worker or could not be sent.
//!
//! # Early outputs
//!
//! An edge with early outputs, such as a crate's metadata, reports each as it writes it, as rustc does with
//! `--json=artifacts`. An edge which needs only those outputs starts as soon as they are reported, rather than when
//! the first edge finishes. The output of such an edge is read as it is written. It is rustc's JSON, and what is shown
//! and kept is what rustc would have printed without it, as for an edge which ran on a worker.
//!
//! # Leaving outputs on workers
//!
//! With [`Download::Targets`], the outputs an edge writes on a worker stay there, apart from its depfile and the
//! targets of the build, including those reached through aliases. An output which is left is stamped by its contents,
//! which its edge's record keeps, while its file is missing here. It is fetched when an edge which runs here, or on
//! another worker, reads it. It is fetched from the cache, the remote cache or the workers.
//!
//! An output which none of them still has is lost. The edge which needed it does not run, the edge which wrote it is
//! recorded as missing that output, and the build is planned and run again, which builds it again. This is repeated a
//! few times at most.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::cache::{self, Cache, Hash};
use crate::remote::Remote;
use crate::graph::{EdgeId, EdgeKind, Exec, Graph, NodeId, Pool};
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
    /// The number of edges which may fail before no more are started, or `0` for no limit.
    pub keep_going: usize,
    /// Whether the commands share the jobs through a jobserver, which rig joins if it is run by one.
    pub jobserver: bool,
    /// Set this to stop the build, as an interrupt does: no more edges start, and those running are sent `SIGINT`.
    pub cancel: Arc<AtomicBool>,
    /// Environment variables every command is run with, beside those rig is run with.
    pub env: Vec<(String, String)>,
    /// The cache the results of cached edges are kept in, if there is one.
    pub cache: Option<Cache>,
    /// The environment variables every hermetic edge inherits, beside those it passes on: `PATH`, `HOME` and `TMPDIR`
    /// by default. They only locate the tools and files an edge uses, and do not change what it produces, so they are
    /// not inputs of the edge.
    pub locating: Vec<String>,
    /// The workers which cached edges may run on, once the build's own jobs are full.
    pub workers: Vec<Arc<Remote>>,
    /// Whether an edge which a worker may run waits for one, rather than running here.
    pub workers_only: bool,
    /// The release of each toolchain, by the toolchain's identity. A worker on another host may run an edge with its
    /// own toolchain of the same release, if the edge's placement allows it, and a worker which installs toolchains
    /// installs the release.
    pub releases: HashMap<String, crate::worker::Release>,
    /// Which outputs of the edges which run on workers are fetched.
    pub download: Download,
    /// What is given to the commands of some edges which run [`always`](crate::Edge::always) beside their actions,
    /// by the first output of each, as a test is given the arguments `jig test` was given. As it is not part of an
    /// edge's action, it is only given to an edge which is never cached nor recorded as up to date.
    pub given: HashMap<String, Given>,
}

/// What [`Options::given`] gives the command of an edge which runs always, beside its action.
#[derive(Clone, Debug, Default)]
pub struct Given {
    /// Arguments after the action's own.
    pub args: Vec<String>,
    /// Environment variables beside the action's, which replace those of the same names.
    pub env: Vec<(String, String)>,
    /// A timeout in place of the edge's own.
    pub timeout: Option<Duration>,
}

/// Which outputs of the edges which run on workers are fetched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Download {
    /// Every one, once its edge has run.
    #[default]
    All,
    /// Only the targets of the build, and what an edge running here or on another worker reads, when it needs it.
    /// The rest stay on the workers.
    Targets,
}
impl Options {
    /// Options for a build in a directory, which stops at the first failure.
    ///
    /// It has slightly more jobs than the machine has threads, as commands spend time waiting on files and processes:
    /// two more, or one more on a machine with one or two threads.
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
            locating: LOCATING.map(String::from).to_vec(),
            workers: Vec::new(),
            workers_only: false,
            releases: HashMap::new(),
            download: Download::All,
            given: HashMap::new(),
        }
    }
}

/// The default of [`Options::locating`]: `PATH`, `HOME` and `TMPDIR`. On Windows it also has the variables for the
/// system's and the user's directories and the extensions of programs, which Windows programs need in order to run.
#[cfg(unix)]
const LOCATING: [&str; 3] = ["PATH", "HOME", "TMPDIR"];
#[cfg(windows)]
const LOCATING: [&str; 11] = [
    "PATH", "HOME", "TMPDIR", "SystemRoot", "SystemDrive", "windir", "TEMP", "TMP", "USERPROFILE", "PATHEXT",
    "ComSpec",
];

/// How an edge which ran finished.
#[derive(Clone, Debug)]
pub enum Outcome {
    Succeeded,
    /// The command exited unsuccessfully.
    Failed(ExitStatus),
    /// The command could not be run, or its depfile could not be read.
    Error(String),
    /// The command did not succeed, but the build had been interrupted, so it is not counted as a failure.
    Interrupted,
    /// The command ran for longer than its timeout, and was killed, with what it started.
    TimedOut(Duration),
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
    /// An edge which was reported as started did not run, as an input it reads was lost. It starts again in the
    /// build's next pass.
    fn unstarted(&mut self, graph: &Graph, edge: EdgeId) {
        let _ = (graph, edge);
    }
    /// Called often while edges run, so that how long they have been running can be shown.
    fn tick(&mut self) {}
    /// The build has finished, or stopped.
    fn done(&mut self) {}
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
    /// The outputs left on workers which the workers no longer had, and which were built again.
    pub lost: usize,
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
    /// An edge which was sent to a worker is given back to run here, as the worker did not run it, or it failed there.
    Returned(Box<Job>),
    /// An edge has reported writing one of its early outputs. The stamp is set if the output was left on a worker.
    Early(usize, NodeId, Option<Stamp>),
}

/// An edge for a thread to run.
struct Job {
    /// The edge's index in the plan.
    index: usize,
    action: Exec,
    /// Whether the edge is in the console pool.
    console: bool,
    /// Where the edge's result is looked for in the cache. If it is found, it is restored and the action does not run.
    lookup: Option<Lookup>,
    /// The variables a hermetic edge inherits, or `None` if it inherits every one.
    inherit: Option<Vec<String>>,
    /// The worker it runs on, if it is sent to one.
    dispatch: Option<Dispatch>,
    /// Set for an edge which failed on a worker: if it succeeds here, it is marked in this cache, under this key, its
    /// first, to run here in future.
    mark: Option<(Cache, Hash)>,
    /// The early outputs it reports, by their paths, if it has any. It then prints rustc's JSON.
    early: Option<Arc<Vec<(String, NodeId)>>>,
    /// The inputs it reads which were left on workers, to fetch before it runs here.
    fetch: Vec<(String, Stamp)>,
    /// For a hashed edge, its outputs and depfile, which the thread that runs it hashes once it has succeeded, to
    /// spare the build's own thread, and the cache they are put in, if it is cached.
    stamp: Option<Arc<(Vec<String>, Option<Cache>)>>,
}

/// A cached edge to send to a worker. The contents of its inputs are hashed by the thread which sends it.
struct Dispatch {
    worker: Arc<Remote>,
    /// The cache its outputs are fetched into.
    cache: Cache,
    /// The edge's first key, under which it is marked to run here if it fails on the worker but succeeds here.
    key: Option<Hash>,
    /// How long it took when it last ran, against which the worker's speed is measured.
    took: Duration,
    identity: String,
    release: String,
    install: String,
    target: String,
    action: Exec,
    env: Vec<(String, String)>,
    locating: Vec<String>,
    inputs: Vec<String>,
    /// The outputs it writes, and its depfile, if it has one.
    outputs: Vec<String>,
    /// The file its stderr is written to, if any, which it need not write.
    stderr: Option<String>,
    /// For an edge which has not run: the action which writes its depfile without running it, and the depfile. The
    /// action runs here first, and the inputs the depfile lists are sent too.
    discovery: Option<(Exec, String)>,
    /// The inputs which were left on workers. Those this worker lacks are fetched here and sent.
    remote: Vec<(String, Stamp)>,
    /// The outputs which are left on the worker, rather than fetched.
    leave: Vec<String>,
    /// The early outputs, which it reports as it writes them, and which are fetched as they are reported.
    early: Vec<String>,
    fetcher: Arc<Fetcher>,
}
/// Outputs left on a worker, by path, with their stamps.
type Left = Vec<(String, Stamp)>;

/// What became of an edge sent to a worker.
enum Sent {
    /// It ran there and its outputs have been restored, except those left there. Holds what it printed, and the
    /// outputs left there.
    Ran(Vec<u8>, Left),
    /// It did not run, as these inputs were left on workers which no longer have them.
    Lost(Vec<String>),
    /// It ran there, and failed, or did not write every output.
    Failed,
    /// It was not run there.
    NotRun,
}

impl Dispatch {
    /// Run the edge on the worker and restore its outputs.
    ///
    /// Each early output is handled as soon as the worker reports it, and `wrote` is then called with its path: with
    /// `None` once the output has been restored here, or with its stamp if it is left on the worker.
    fn run(&self, dir: &Path, wrote: &(dyn Fn(&str, Option<Stamp>) + Sync)) -> Sent {
        match self.try_run(dir, wrote) {
            Ok((printed, left)) => Sent::Ran(printed, left),
            Err(sent) => sent,
        }
    }
    fn try_run(&self, dir: &Path, wrote: &(dyn Fn(&str, Option<Stamp>) + Sync)) -> Result<(Vec<u8>, Left), Sent> {
        let mut paths = self.inputs.clone();
        // Inputs left on workers are fetched here if this worker lacks them, to be sent to it. They are all fetched
        // if the discovery action runs, as it runs here and may read any of them.
        if !self.remote.is_empty() {
            let fetch = if self.discovery.is_some() {
                self.remote.clone()
            } else {
                let blobs: Vec<Hash> = self.remote.iter().filter_map(|(_, stamp)| stamp.hash().copied()).collect();
                let missing = self.worker.missing(&blobs).ok_or(Sent::NotRun)?;
                let missing = |stamp: &Stamp| stamp.hash().is_some_and(|hash| missing.contains(hash));
                self.remote.iter().filter(|(_, stamp)| missing(stamp)).cloned().collect()
            };
            let lost = self.fetcher.fetch_all(&fetch, 1);
            if !lost.is_empty() {
                return Err(Sent::Lost(lost));
            }
        }
        // An edge which has not run discovers its inputs here first. If that fails, the edge runs here, which shows
        // the error.
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
            // An input which is not a file here is not sent, unless it was left on a worker, when this worker has it
            // by now. An action which reads an input that was not sent fails.
            let Some(metadata) = std::fs::metadata(&full).ok().filter(std::fs::Metadata::is_file) else {
                let left = self.remote.iter().find(|(remote, _)| remote == path);
                if let Some(&(_, Stamp::Remote { hash, executable })) = left {
                    inputs.push(crate::worker::Input { path: path.clone(), blob: hash, executable });
                }
                continue;
            };
            let blob = crate::blake3::hash_file(&full).map_err(|_| Sent::NotRun)?;
            let executable = sys::is_executable(&metadata);
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
            early: self.early.clone(),
        };
        // An early output is fetched as soon as it is reported, as edges which need only it are waiting. If it is
        // to be left on the worker instead, the file here is stale, and is removed.
        let early = |output: cache::Output| {
            let full = dir.join(&output.path);
            if self.leave.contains(&output.path) {
                let _ = std::fs::remove_file(&full);
                return wrote(&output.path, Some(Stamp::Remote { hash: output.blob, executable: output.executable }));
            }
            if !self.cache.has_blob(&output.blob) {
                self.worker.fetch_blobs(&self.cache, &[output.blob]);
            }
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if self.cache.restore(&output.blob, &full, output.executable).is_ok() {
                wrote(&output.path, None);
            }
        };
        let (executed, ran) = self.worker.execute_timed(dir, &request, &early).map_err(|_| Sent::NotRun)?;
        if executed.succeeded {
            self.worker.timed(ran, self.took);
        }
        let written = |path: &String| executed.entry.outputs.iter().any(|output| output.path == *path);
        if !executed.succeeded || !self.outputs.iter().all(written) {
            return Err(Sent::Failed);
        }
        // an output which may be left on the worker is left, unless the same file is already here
        let (mut kept, mut left) = (Vec::new(), Vec::new());
        for output in executed.entry.outputs {
            let here = || crate::blake3::hash_file(&dir.join(&output.path)).is_ok_and(|hash| hash == output.blob);
            if !self.leave.contains(&output.path) {
                kept.push(output);
            } else if !here() {
                left.push(output);
            }
        }
        let entry = cache::Entry { outputs: kept, printed: executed.entry.printed };
        let blobs = cache::entry_blobs(&entry);
        let missing: Vec<Hash> = blobs.filter(|blob| !self.cache.has_blob(blob)).copied().collect();
        let fetching = Instant::now();
        let fetched = self.worker.fetch_blobs(&self.cache, &missing);
        let size = |blob: &Hash| std::fs::metadata(self.cache.blob_path(blob)).map_or(0, |metadata| metadata.len());
        self.worker.measured(fetched.iter().map(size).sum(), fetching.elapsed());
        let printed = Restore { cache: &self.cache, entry }.apply(dir).map_err(|_| Sent::NotRun)?;
        // remove what is here of an output left there, which is stale
        for output in &left {
            let _ = std::fs::remove_file(dir.join(&output.path));
        }
        let left = left.into_iter().map(|output| {
            (output.path, Stamp::Remote { hash: output.blob, executable: output.executable })
        });
        Ok((printed, left.collect()))
    }
    /// Returns the cache and key under which the edge is marked to run here. It is marked if it fails on the worker
    /// and then succeeds here, which shows that it depends on something it neither declares nor discovers. The mark
    /// lasts until its first key changes.
    fn mark(&self) -> Option<(Cache, Hash)> {
        Some((self.cache.clone(), self.key?))
    }
}

/// Where the result of a cached edge is looked for.
///
/// The lookup runs on the thread which runs the edge, not the build's thread, so that edges waiting on a remote cache
/// wait at the same time. It therefore hashes the inputs the edge discovered itself, rather than using the build's
/// [`Stamps`].
struct Lookup {
    cache: Cache,
    /// The edge's first key.
    key: Hash,
    /// The variables every command is given, which the discovered variables are read through.
    env: Arc<Vec<(String, String)>>,
    /// The outputs left on workers, whose hashes are known while their files are missing here.
    left: Arc<RwLock<HashMap<String, Stamp>>>,
}
impl Lookup {
    /// Returns the second key for a set of inputs and variables the edge once discovered: the hash of each input's
    /// contents now, or that it is missing, and of each variable's value as a command sees it. The inputs are
    /// relative to the build's directory.
    fn second_key(&self, dir: &Path, discovery: &cache::Discovery) -> Hash {
        let hash = |path: &String| {
            let left = || self.left.read().ok()?.get(path)?.hash().copied();
            crate::blake3::hash_file(&dir.join(path)).ok().or_else(left)
        };
        let hashes: Vec<_> = discovery.inputs.iter().map(hash).collect();
        let inputs: Vec<_> = discovery.inputs.iter().zip(&hashes).map(|(path, hash)| (path.as_str(), hash.as_ref())).collect();
        let value = |name: &str| match self.env.iter().rev().find(|(variable, _)| variable == name) {
            Some((_, value)) => Some(crate::blake3::hash(value.as_bytes())),
            None => std::env::var_os(name).map(|value| crate::blake3::hash(value.as_encoded_bytes())),
        };
        let values: Vec<_> = discovery.env.iter().map(|name| value(name)).collect();
        let env: Vec<_> = discovery.env.iter().zip(&values).map(|(name, hash)| (name.as_str(), hash.as_ref())).collect();
        cache::second_key(&self.key, &inputs, &env)
    }
    /// Find the edge's result and restore it. Returns what its command printed, or `None` if the result is not found
    /// or cannot be restored.
    fn restore(&self, dir: &Path) -> Option<Vec<u8>> {
        let (_, entry) = self.cache.find(&self.key, |discovery| self.second_key(dir, discovery))?;
        Restore { cache: &self.cache, entry }.apply(dir).ok()
    }
}

/// A cached result of an edge, to restore its outputs from.
struct Restore<'a> {
    cache: &'a Cache,
    entry: cache::Entry,
}
impl Restore<'_> {
    /// Restore the outputs of the edge, relative to the build's directory, and return what its command printed. The
    /// blobs missing from the cache are first fetched from the remote cache, in one request.
    ///
    /// If an output cannot be restored, every blob of the result is checked, so that corrupted ones are removed, to
    /// be added again once the edge has run.
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

/// Fetches outputs which were left on workers, when they are needed here, to their files. Each is fetched once, from
/// the cache, which itself asks the remote cache, or failing that from the workers.
struct Fetcher {
    dir: PathBuf,
    cache: Option<Cache>,
    workers: Vec<Arc<Remote>>,
    /// The result of fetching each file asked for so far, by its path, set once the fetch has finished.
    fetched: Mutex<HashMap<String, Arc<OnceLock<bool>>>>,
}
impl Fetcher {
    /// Fetch a file if its stamp says it was left on a worker. Returns whether the file is now here, which is `true`
    /// for a file that was never left.
    fn fetch(&self, path: &str, stamp: Stamp) -> bool {
        let Stamp::Remote { hash, executable } = stamp else {
            return true;
        };
        let Some(once) = self.fetched.lock().ok().map(|mut fetched| Arc::clone(fetched.entry(path.into()).or_default()))
        else {
            return false;
        };
        *once.get_or_init(|| {
            let Some(cache) = &self.cache else {
                return false;
            };
            cache.fetch_blobs(&[hash]);
            for worker in &self.workers {
                if cache.has_blob(&hash) {
                    break;
                }
                if worker.usable() {
                    worker.fetch_blobs(cache, &[hash]);
                }
            }
            let full = self.dir.join(path);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            cache.restore(&hash, &full, executable).is_ok()
        })
    }
    /// Returns the files which were fetched.
    fn fetched(&self) -> Vec<String> {
        let Ok(fetched) = self.fetched.lock() else {
            return Vec::new();
        };
        let fetched = fetched.iter().filter(|(_, once)| once.get() == Some(&true));
        fetched.map(|(path, _)| path.clone()).collect()
    }
    /// Fetch files left on workers, on up to `threads` threads. Returns those which could not be fetched.
    fn fetch_all(&self, files: &[(String, Stamp)], threads: usize) -> Vec<String> {
        let lost = |files: &[(String, Stamp)]| -> Vec<String> {
            let lost = files.iter().filter(|(path, stamp)| !self.fetch(path, *stamp));
            lost.map(|(path, _)| path.clone()).collect()
        };
        if threads <= 1 || files.len() <= 1 {
            return lost(files);
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> =
                files.chunks(files.len().div_ceil(threads)).map(|chunk| scope.spawn(move || lost(chunk))).collect();
            handles.into_iter().flat_map(|handle| handle.join().unwrap_or_default()).collect()
        })
    }
}

/// Stamp the files an edge wrote, relative to the build's directory, by their contents, and add them to the cache if
/// one is given. Returns the stamps of those which are files. A file which cannot be read is skipped, and left for
/// the build's own thread to stamp.
fn stamp_outputs(dir: &Path, paths: &[String], cache: Option<&Cache>) -> Vec<(String, Stamp)> {
    let mut stamped = Vec::new();
    for path in paths {
        let full = dir.join(path);
        let Stamp::File { mtime, size } = Stamp::of(&full) else {
            continue;
        };
        let Ok(hash) = crate::blake3::hash_file(&full) else {
            continue;
        };
        // adding the file now only saves time later, so a failure is ignored: it is added when the result is stored
        if let Some(cache) = cache {
            let _ = cache.put_file(&full, &hash);
        }
        stamped.push((path.clone(), Stamp::Content { mtime, size, hash }));
    }
    stamped
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
    /// The outputs it left on the worker, with their stamps.
    left: Vec<(String, Stamp)>,
    /// The inputs it reads which were left on workers that no longer have them. If there are any, it did not run.
    lost: Vec<String>,
    /// Its outputs and depfile, stamped by their contents by the thread which ran it, if it is hashed.
    stamped: Vec<(String, Stamp)>,
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

/// The most times a build is planned and run. It is run again only if outputs left on workers were lost.
const PASSES: usize = 3;

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
    let mut total = Report::default();
    let keeping = keep_left(state, options);
    for _ in 0..PASSES {
        let report = build_once(graph, state, options, targets, observer);
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                observer.done();
                return Err(error);
            },
        };
        total.ran += report.ran;
        total.failed += report.failed;
        total.restat_skipped += report.restat_skipped;
        total.cached += report.cached;
        total.remote += report.remote;
        total.lost += report.lost;
        (total.not_run, total.interrupted) = (report.not_run, report.interrupted);
        // run again only to rebuild outputs which were lost, and what needed them
        if report.lost == 0 || report.interrupted || report.failed > 0 {
            break;
        }
    }
    if let Some(keeping) = keeping {
        let _ = keeping.join();
    }
    observer.done();
    Ok(total)
}

/// Ask the workers which of the files left on them they are missing, on a separate thread, while the build goes on.
/// The answers are not used: asking marks the files as used, which keeps the workers' caches from collecting them.
fn keep_left(state: &State, options: &Options) -> Option<std::thread::JoinHandle<()>> {
    if options.workers.is_empty() {
        return None;
    }
    let mut blobs: Vec<Hash> = state.remote_outputs().values().filter_map(|stamp| stamp.hash().copied()).collect();
    if blobs.is_empty() {
        return None;
    }
    blobs.sort_unstable();
    blobs.dedup();
    let workers = options.workers.clone();
    let keep = move || {
        for worker in workers.iter().filter(|worker| worker.usable()) {
            let _ = worker.missing(&blobs);
        }
    };
    Some(std::thread::spawn(keep))
}

/// Plan and run a build once.
fn build_once(
    graph: &Graph,
    state: &mut State,
    options: &Options,
    targets: &[&str],
    observer: &mut dyn Observer,
) -> Result<Report, Error> {
    let mut stamps = Stamps::new(graph, &options.dir).with_env(&options.env);
    let plan = plan::plan(graph, state, &mut stamps, targets)?;
    let wanted = wanted(graph, &plan::target_nodes(graph, targets)?);
    let left = Arc::new(RwLock::new(state.remote_outputs()));
    let fetcher = Arc::new(Fetcher {
        dir: options.dir.clone(),
        cache: options.cache.clone(),
        workers: options.workers.clone(),
        fetched: Mutex::default(),
    });
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
    let estimates = if options.workers.is_empty() { HashMap::new() } else { estimates(graph, state) };
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
        on_workers: vec![HashSet::new(); options.workers.len()],
        sent_to: HashMap::new(),
        estimates,
        wanted,
        left,
        fetcher,
        lost: Vec::new(),
        serial: false,
        timeouts: HashMap::new(),
        timed_out: HashMap::new(),
    };
    if options.jobserver && !run.edges.is_empty() {
        run.jobserver = Jobserver::from_env().or_else(|| Jobserver::create(options.jobs.max(1)).ok());
    }
    // find the pairs where an edge needs only the early outputs of an edge it depends on
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
    // the targets which were left on workers are fetched
    if ran.is_ok() && !run.report.interrupted {
        run.fetch_targets();
    }
    run.record_fetched();
    // what was added to the cache is pushed to the remote cache before the build ends
    if let Some(cache) = &options.cache {
        cache.wait_for_pushes();
    }
    ran?;
    if let Some(error) = run.error.take() {
        return Err(error);
    }
    run.report.not_run = run.status.iter().filter(|status| matches!(status, Status::Waiting)).count();
    run.report.lost = run.lost.len();
    Ok(run.report)
}

/// Returns the files to fetch if they were left on workers: the targets, with the inputs of any alias among them in
/// its place, however deeply aliases nest.
fn wanted(graph: &Graph, targets: &[NodeId]) -> HashSet<NodeId> {
    let mut wanted = HashSet::new();
    let mut seen = HashSet::new();
    let mut stack = targets.to_vec();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) {
            continue;
        }
        match graph.producer(node).map(|edge| graph.edge(edge)) {
            Some(alias) if *alias.kind() == EdgeKind::Alias => {
                stack.extend(alias.inputs().iter().chain(alias.dependencies()).chain(alias.after()));
            },
            _ => {
                wanted.insert(node);
            },
        }
    }
    wanted
}

/// Returns, for each toolchain identity, the median time the cached edges with that toolchain took when they last
/// ran, and the median size of their outputs. An edge which has not run is estimated from them.
fn estimates(graph: &Graph, state: &State) -> HashMap<String, (Duration, u64)> {
    let mut found: HashMap<&str, (Vec<u32>, Vec<u64>)> = HashMap::new();
    for (id, edge) in graph.edges().enumerate() {
        let Some(identity) = edge.cache() else {
            continue;
        };
        let id = EdgeId(u32::try_from(id).unwrap_or(u32::MAX));
        let Some(record) = state.get(plan::first_output(graph, id)) else {
            continue;
        };
        let (took, sizes) = found.entry(identity).or_default();
        took.push(record.duration);
        sizes.push(record.outputs.iter().map(|(_, stamp)| stamp.size()).sum());
    }
    let median = |values: &mut Vec<u64>| {
        values.sort_unstable();
        values[values.len() / 2]
    };
    let estimates = found.into_iter().map(|(identity, (took, mut sizes))| {
        let mut took: Vec<u64> = took.into_iter().map(u64::from).collect();
        (identity.to_string(), (Duration::from_millis(median(&mut took)), median(&mut sizes)))
    });
    estimates.collect()
}

/// Returns the priority of each edge in the plan: the time of the longest path from it to the end of the build. That
/// is how long it took when it last ran, in milliseconds, or one if it has not run, plus the highest priority among
/// the edges which depend on it. Edges on the longest, critical, path run first, so that a long chain does not start
/// last.
///
/// The plan lists each edge after those it depends on, so the priorities are computed from its end.
fn critical_paths(graph: &Graph, state: &State, edges: &[EdgeId], dependents: &[Vec<usize>]) -> Vec<u64> {
    let mut priority = vec![0; edges.len()];
    for index in (0..edges.len()).rev() {
        let record = state.get(plan::first_output(graph, edges[index]));
        let duration = record.map_or(1, |record| u64::from(record.duration).max(1));
        let longest = dependents[index].iter().map(|&dependent| priority[dependent]).max().unwrap_or(0);
        priority[index] = duration.saturating_add(longest);
    }
    priority
}

/// Whether `edge` needs only the early outputs of an edge it depends on: every input of it, declared or discovered,
/// which the other edge produces is an early output. An input which is an alias rules this out, as the alias may
/// stand for the other outputs.
fn early_only(graph: &Graph, state: &State, dependent: EdgeId, dependency: EdgeId) -> bool {
    let early = graph.edge(dependency).early();
    let data = graph.edge(dependent);
    let declared = data.inputs().iter().chain(data.dependencies()).chain(data.after());
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

/// Whether two paths relative to the build's directory name the same file, ignoring a leading `./`, and on Windows,
/// whether components are separated by `\\` or `/`. rustc reports an artifact by the path it was given, which on
/// Windows jig gives with `\\`, while the graph has `/`.
pub(crate) fn same_path(lhs: &str, rhs: &str) -> bool {
    let trim = |path: &str| {
        let path = if cfg!(windows) { path.replace('\\', "/") } else { path.to_string() };
        path.trim_start_matches("./").to_string()
    };
    trim(lhs) == trim(rhs)
}

/// Returns the edges in the plan which produce an edge's inputs, declared and discovered, looking through aliases.
fn planned_deps(graph: &Graph, state: &State, edge: EdgeId, index: &HashMap<EdgeId, usize>) -> Vec<usize> {
    let data = graph.edge(edge);
    let inputs = data.inputs().iter().chain(data.dependencies()).chain(data.after());
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
            nodes.extend(alias.inputs().iter().chain(alias.dependencies()).chain(alias.after()));
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
    /// The edges of the plan, in its order: each comes after those it depends on.
    edges: Vec<EdgeId>,
    reasons: Vec<Reason>,
    index: HashMap<EdgeId, usize>,
    /// For each edge, the edges of the plan it depends on, and those which depend on it.
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
    /// The jobserver token each running edge holds. One edge runs without a token, in the build's own job.
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
    /// The build's absolute directory, or empty if it is not known. The cache keys of edges have a marker in its
    /// place, so that a build in another directory shares their results.
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
    /// Set once the build is interrupted, so that an edge which was interrupted on a worker does not then run here.
    stop: Arc<AtomicBool>,
    /// The edges which workers gave back, in the order they were given. Each runs here once a job is free.
    returned: std::collections::VecDeque<Job>,
    /// The pairs of a dependent and an edge it depends on where the dependent needs only the edge's early outputs.
    early_deps: HashSet<(usize, usize)>,
    /// The early outputs each running edge has reported.
    reported: HashMap<usize, HashSet<NodeId>>,
    /// The pairs of a dependent and an edge it depends on where the dependent has stopped waiting for the edge, as
    /// the edge has reported its early outputs.
    released: HashSet<(usize, usize)>,
    /// When each running edge started.
    started: HashMap<usize, Instant>,
    /// The files each worker has from this build, as it wrote them or was sent them. They are not sent to it again.
    on_workers: Vec<HashSet<String>>,
    /// The worker each edge which was sent to one was sent to, until it finishes.
    sent_to: HashMap<usize, usize>,
    /// The median time and size of outputs of the cached edges of each toolchain, as [`estimates`] returns them.
    /// It is empty if there are no workers.
    estimates: HashMap<String, (Duration, u64)>,
    /// The files which are fetched if they were left on workers.
    wanted: HashSet<NodeId>,
    /// The stamps of the outputs left on workers whose files are missing here, by path. The threads which look edges
    /// up in the cache read it.
    left: Arc<RwLock<HashMap<String, Stamp>>>,
    fetcher: Arc<Fetcher>,
    /// The outputs left on workers which the workers no longer had. They are built again.
    lost: Vec<String>,
    /// Whether an edge in the serial pool is running, while nothing else starts.
    serial: bool,
    /// The timeout of each running edge which has one, which runs here outside the console pool.
    timeouts: HashMap<usize, Duration>,
    /// The running edges which were killed as they ran for longer than their timeouts, with the timeouts.
    timed_out: HashMap<usize, Duration>,
}

impl Run<'_, '_> {
    fn run(&mut self, observer: &mut dyn Observer) -> Result<(), Error> {
        let (sender, receiver) = std::sync::mpsc::channel::<Message>();
        let interrupts = sys::Interrupts::catch();
        let makeflags = self.jobserver.as_ref().and_then(Jobserver::makeflags).map(String::from);
        // when the build was interrupted, until its processes are killed
        let mut interrupted_at = None;
        // there is one thread for each job, not one for each edge
        let (jobs, work) = std::sync::mpsc::channel::<Job>();
        let work = std::sync::Mutex::new(work);
        let (dir, env) = (self.options.dir.clone(), self.options.env.clone());
        let (work, dir, env, makeflags) = (&work, &dir, &env, makeflags.as_deref());
        // The body of a thread which takes jobs. One is started for each of the build's jobs, and one for each of a
        // worker's jobs when the worker is first sent an edge.
        let stop = Arc::clone(&self.stop);
        let stop = &stop;
        let fetcher = Arc::clone(&self.fetcher);
        let fetcher = &fetcher;
        let job_thread = || {
            let sender = sender.clone();
            move || {
                // the lock is held while waiting for a job, so the threads take jobs one at a time
                while let Ok(Ok(job)) = work.lock().map(|work| work.recv()) {
                    let Job { index, action, console, lookup, inherit, dispatch, mark, early, fetch, stamp } = job;
                    let start = SystemTime::now();
                    let clock = Instant::now();
                    // a result which cannot be restored, as when the cache is corrupted, is a miss: the action runs
                    let restored = lookup.and_then(|lookup| lookup.restore(dir));
                    let cached = restored.is_some();
                    // pass on each early output as it is reported, with its stamp if it was left on a worker
                    let reported_as = |path: &str, left: Option<Stamp>| {
                        let outputs = early.iter().flat_map(|early| early.iter());
                        let reported = outputs.filter(|(output, _)| same_path(output, path));
                        for &(_, node) in reported {
                            let _ = sender.send(Message::Early(index, node, left));
                        }
                    };
                    let reported = |path: &str| reported_as(path, None);
                    let sent = match &dispatch {
                        Some(dispatch) if !cached => dispatch.run(dir, &reported_as),
                        _ => Sent::NotRun,
                    };
                    let (mut left, mut lost) = (Vec::new(), Vec::new());
                    let (ran, failed_there) = match sent {
                        // what the worker sends is the edge's raw output, which is rendered as it would be here
                        Sent::Ran(printed, kept) if early.is_some() => {
                            left = kept;
                            (Some(crate::json::rendered(&printed, &mut |_| ())), false)
                        },
                        Sent::Ran(printed, kept) => {
                            left = kept;
                            (Some(printed), false)
                        },
                        Sent::Lost(paths) => {
                            lost = paths;
                            (None, false)
                        },
                        Sent::Failed => (None, true),
                        Sent::NotRun => (None, false),
                    };
                    let remote = ran.is_some();
                    // an edge which the worker did not run is given back to run in one of the build's own jobs,
                    // unless it was interrupted on the worker
                    let given_back = !cached && !remote && lost.is_empty() && !stop.load(Ordering::SeqCst);
                    if let Some(dispatch) = dispatch.as_ref().filter(|_| given_back) {
                        let mark = dispatch.mark().filter(|_| failed_there);
                        let (lookup, dispatch) = (None, None);
                        let job = Job { index, action, console, lookup, inherit, dispatch, mark, early, fetch, stamp };
                        let _ = sender.send(Message::Returned(Box::new(job)));
                        continue;
                    }
                    // before it runs here, fetch the inputs it reads which were left on workers
                    if !cached && !remote && dispatch.is_none() {
                        lost = fetcher.fetch_all(&fetch, 1);
                    }
                    let mut ran_here = false;
                    let (outcome, output) = if !lost.is_empty() {
                        (Outcome::Error(format!("{} were left on workers which no longer have them", lost.join(" "))),
                            Vec::new())
                    } else if let Some(printed) = restored.or(ran) {
                        (Outcome::Succeeded, printed)
                    } else if dispatch.is_some() {
                        // an edge which was interrupted on a worker does not run here
                        (Outcome::Interrupted, Vec::new())
                    } else {
                        let spawned = |pid| {
                            let _ = sender.send(Message::Spawned(index, pid));
                        };
                        let reports: Option<&dyn Fn(&str)> = early.as_ref().map(|_| &reported as &dyn Fn(&str));
                        let environment = Environment { makeflags, env, inherit: inherit.as_deref(), reports };
                        ran_here = true;
                        run_command(&action, dir, console, &environment, &spawned)
                    };
                    // hash what it wrote, and add it to the cache, on this thread, to spare the build's own
                    let stamped = match stamp.as_deref() {
                        Some((paths, cache)) if ran_here && outcome.succeeded() => {
                            stamp_outputs(dir, paths, cache.as_ref())
                        },
                        _ => Vec::new(),
                    };
                    if outcome.succeeded() && let Some((cache, key)) = &mark {
                        cache.mark_local(key);
                    }
                    let duration = clock.elapsed();
                    let finished =
                        Finished { index, outcome, output, start, duration, cached, remote, left, lost, stamped };
                    let _ = sender.send(Message::Finished(finished));
                }
            }
        };
        std::thread::scope(|scope| {
            for _ in 0..self.options.jobs.max(1) {
                scope.spawn(job_thread());
            }
            // start connecting to the workers straight away, if some edge may run on one
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
                // kill a process which ignores the signal, as a shell's background job ignores `SIGINT`
                if let Some(at) = interrupted_at && at.elapsed() >= KILL_AFTER {
                    interrupted_at = None;
                    for &group in self.processes.values() {
                        sys::signal_group(group, sys::SIGKILL);
                    }
                }
                // kill each edge, with what it started, once it has run for longer than its timeout
                for (&index, &timeout) in &self.timeouts {
                    let late = self.started.get(&index).is_some_and(|started| started.elapsed() >= timeout);
                    if late && let Some(&group) = self.processes.get(&index) && !self.timed_out.contains_key(&index) {
                        sys::signal_group(group, sys::SIGKILL);
                        self.timed_out.insert(index, timeout);
                    }
                }
                // whether an edge is ready but waits for a token
                let mut waiting_for_token = false;
                // An edge a worker gave back is first to take a job, as it was already reported as started. It runs
                // even if the build has stopped starting edges, unless the build was interrupted.
                if self.report.interrupted {
                    for job in std::mem::take(&mut self.returned) {
                        let now = SystemTime::now();
                        let outcome = Outcome::Interrupted;
                        let finished = Finished { index: job.index, outcome, output: Vec::new(), start: now,
                            duration: Duration::ZERO, cached: false, remote: false, left: Vec::new(),
                            lost: Vec::new(), stamped: Vec::new() };
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
                // nothing starts while an edge in the serial pool runs
                while !self.stopping() && !self.serial {
                    // An edge runs here while the build has a free job, and otherwise on a worker, if one can run
                    // it. An edge which waits for a worker never runs here.
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
                    // An edge running here uses the job the build holds without a token, if that is free, and
                    // otherwise takes a token. An edge on a worker needs neither.
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
                    let action = self.given_action(index);
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
                    // some thread is free to take it, as there is a thread for each job and no more edges than jobs
                    let early = self.early_outputs(index);
                    let fetch = self.remote_inputs(index);
                    let (mark, stamp) = (None, self.stamp_outputs(index));
                    let job = Job { index, action, console, lookup, inherit, dispatch, mark, early, fetch, stamp };
                    let _ = jobs.send(job);
                }
                // the build goes on while an edge waits for a worker which is connecting
                if self.progress.running == 0 && !self.waiting_for_workers() {
                    break;
                }
                // other commands sharing the jobserver return tokens too, so tokens are polled for often, and an
                // interrupt less often
                let timeout = Duration::from_millis(if waiting_for_token { 5 } else { 50 });
                observer.tick();
                match receiver.recv_timeout(timeout) {
                    Ok(Message::Spawned(index, pid)) => {
                        if self.graph.edge(self.edges[index]).pool() != Pool::Console {
                            self.processes.insert(index, pid);
                            if self.report.interrupted {
                                sys::signal_group(pid, sys::SIGINT);
                            }
                        }
                    },
                    Ok(Message::Early(index, node, left)) => self.early(index, node, left),
                    Ok(Message::Returned(job)) => {
                        let job = *job;
                        // the worker's job is free, and the edge is not sent to one again
                        if let Some(worker) = self.sent.remove(&job.index) {
                            self.on_worker[worker] -= 1;
                        }
                        self.local_only.insert(job.index);
                        self.returned.push_back(job);
                    },
                    Ok(Message::Finished(mut finished)) => {
                        self.processes.remove(&finished.index);
                        self.timeouts.remove(&finished.index);
                        // a command which was killed as it ran too long failed by its timeout, unless it succeeded
                        if let Some(timeout) = self.timed_out.remove(&finished.index)
                            && !finished.outcome.succeeded()
                        {
                            finished.outcome = Outcome::TimedOut(timeout);
                        }
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
    /// Whether to start no more edges, as too many have failed or the state could not be written.
    fn stopping(&self) -> bool {
        let failed = self.options.keep_going != 0 && self.report.failed >= self.options.keep_going;
        failed || self.error.is_some() || self.report.interrupted
    }
    /// Returns the highest-priority edge which is ready to run here. That excludes an edge in the console pool while
    /// another console edge runs, an edge in the serial pool while anything runs, and an edge which is waiting for a
    /// worker.
    fn next_ready(&mut self) -> Option<usize> {
        let entry = *self.ready.iter().find(|&&(_, index)| {
            let pool = self.graph.edge(self.edges[index]).pool();
            let console = self.console && pool == Pool::Console;
            let serial = pool == Pool::Serial && self.progress.running > 0;
            !console && !serial && !self.waits_for_workers(index)
        })?;
        self.ready.remove(&entry);
        Some(entry.1)
    }
    /// Whether an edge waits for a worker rather than running here. That is so with [`Options::workers_only`], for an
    /// edge which may run on a worker, while some worker is still connecting or has reported that it can run it.
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
    /// Whether a worker may run an edge, going by what the worker reported of itself, and has not refused the edge's
    /// toolchain.
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
    /// Returns the highest-priority edge which is ready and may run on a worker, with the worker to run it on: one
    /// with a free job which may run it, going by what the worker reported of itself. Returns `None` if there is no
    /// such pair.
    fn next_for_worker(&mut self) -> Option<(usize, usize)> {
        // each worker is connected to on a thread of its own, so that the build does not wait for it
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
            // choose the worker it would finish soonest on, counting the time to send it and to run it there
            let took = self.estimated(index);
            let there = |worker: usize| {
                let link = self.options.workers[worker].link();
                self.sending(index, worker, link).saturating_add(link.running(took))
            };
            let candidates = (0..self.options.workers.len())
                .filter(|&worker| free(self, worker) && self.runs(index, worker));
            let worker = candidates.min_by_key(|&worker| there(worker));
            // if it would finish sooner here, it waits for a job here, unless it only runs on workers
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
    /// Estimates how long an edge would wait for one of the build's own jobs: the least time any edge running here has
    /// left, going by how long each took when it last ran. Returns `None` if none of them has run before.
    fn local_wait(&self) -> Option<Duration> {
        let running = self.started.iter().filter(|(index, _)| !self.sent.contains_key(index));
        let left = running.filter_map(|(&index, started)| {
            let record = self.state.get(plan::first_output(self.graph, self.edges[index]))?;
            Some(Duration::from_millis(u64::from(record.duration)).saturating_sub(started.elapsed()))
        });
        left.min()
    }
    /// Returns how long an edge took when it last ran, or nothing if it has not run.
    fn took(&self, index: usize) -> Duration {
        let record = self.state.get(plan::first_output(self.graph, self.edges[index]));
        record.map_or(Duration::ZERO, |record| Duration::from_millis(u64::from(record.duration)))
    }
    /// Estimates how long it would take to send an edge to a worker and fetch its outputs. That is the time to
    /// transfer its files, at the sizes they had when it last ran and leaving out those the worker has, plus the
    /// round trips: asking which blobs the worker is missing, sending them, running the edge and fetching its outputs.
    fn sending(&self, index: usize, worker: usize, link: crate::remote::Link) -> Duration {
        let has = &self.on_workers[worker];
        let sent = |(path, stamp): &(String, Stamp)| if has.contains(path) { 0 } else { stamp.size() };
        let Some(previous) = self.state.get(plan::first_output(self.graph, self.edges[index])) else {
            // An edge which has not run is estimated to send its declared inputs at their sizes now, and to fetch
            // outputs of the median size for its toolchain, if other edges with that toolchain have run.
            let identity = self.graph.edge(self.edges[index]).cache().unwrap_or_default();
            let Some(&(_, outputs)) = self.estimates.get(identity) else {
                return Duration::MAX;
            };
            let inputs: u64 = self.inputs.get(&index).into_iter().flatten().map(sent).sum();
            return link.time(inputs.saturating_add(outputs), 4);
        };
        let files = previous.inputs.iter().chain(&previous.discovered).chain(&previous.outputs);
        link.time(files.map(sent).sum(), 4)
    }
    /// Estimates how long an edge takes to run: as long as it last took, or if it has not run, the median for the
    /// edges with its toolchain. Returns `None` if none of those has run either.
    fn estimated(&self, index: usize) -> Duration {
        let edge = self.edges[index];
        if self.state.get(plan::first_output(self.graph, edge)).is_some() {
            return self.took(index);
        }
        let identity = self.graph.edge(edge).cache().unwrap_or_default();
        self.estimates.get(identity).map_or(Duration::ZERO, |&(took, _)| took)
    }
    /// Returns the toolchain identity of an edge if the edge may run on a worker, which requires all of:
    ///
    /// - it is cached, and not in the console pool;
    /// - it inherits only the variables which locate its tools;
    /// - its inputs are known, as it has run before, or it has an action which discovers them;
    /// - the paths of its inputs and outputs can all be expressed relative to the build's directory.
    fn remote_identity(&self, index: usize) -> Option<&str> {
        let edge = self.graph.edge(self.edges[index]);
        let identity = edge.cache()?;
        if edge.pool() == Pool::Console || !edge.hermetic() || self.options.cache.is_none() {
            return None;
        }
        let outputs = edge.outputs().iter().map(|&node| self.graph.path(node));
        let outputs: Vec<&str> = outputs.chain(edge.depfile()).collect();
        let leads = |path: &str| crate::worker::ups(path).is_some();
        // an edge which has not run can still be sent if it has an action which discovers its inputs
        let Some(previous) = self.state.get(plan::first_output(self.graph, self.edges[index])) else {
            let declared = edge.inputs().iter().chain(edge.dependencies()).map(|&node| self.graph.path(node));
            let discovers = edge.discover().is_some() && edge.depfile().is_some();
            return (discovers && declared.chain(outputs).all(leads)).then_some(identity);
        };
        let recorded = previous.inputs.iter().chain(&previous.discovered).map(|(path, _)| path.as_str());
        recorded.chain(outputs).all(leads).then_some(identity)
    }
    /// Returns how to run a started edge on a worker, with the inputs it declares now and those it discovered when it
    /// last ran. Returns `None`, so that the edge runs here, if an input cannot be expressed relative to the build's
    /// directory, or the edge is marked to run here.
    fn dispatch(&mut self, index: usize, worker: usize) -> Option<Dispatch> {
        let edge = self.graph.edge(self.edges[index]);
        let cache = self.options.cache.as_ref()?.without_remote();
        let key = self.keys.get(&index).copied();
        if key.is_some_and(|key| cache.is_local(&key)) {
            self.local_only.insert(index);
            return None;
        }
        let previous = self.state.get(plan::first_output(self.graph, self.edges[index]));
        // an edge which has not run discovers its inputs when it is sent
        let discovery = match previous {
            Some(_) => None,
            None => Some((edge.discover()?.clone(), edge.depfile()?.to_string())),
        };
        let declared = self.inputs.get(&index)?.iter().map(|(path, _)| path);
        let discovered = previous.into_iter().flat_map(|previous| previous.discovered.iter().map(|(path, _)| path));
        let mut inputs: Vec<String> = declared.chain(discovered).cloned().collect();
        inputs.sort_unstable();
        inputs.dedup();
        let outputs = edge.outputs().iter().map(|&node| self.graph.path(node));
        let outputs: Vec<String> = outputs.chain(edge.depfile()).map(String::from).collect();
        let stderr = self.action(index).stderr.clone();
        if !inputs.iter().chain(&outputs).chain(&stderr).all(|path| crate::worker::ups(path).is_some()) {
            self.local_only.insert(index);
            return None;
        }
        // the worker will have everything it is sent
        self.on_workers[worker].extend(inputs.iter().cloned());
        self.sent_to.insert(index, worker);
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
            remote: self.remote_inputs(index),
            leave: self.leaves(index),
            early: edge.early().iter().map(|&node| self.graph.path(node).to_string()).collect(),
            fetcher: Arc::clone(&self.fetcher),
        })
    }
    /// Returns what the thread which runs a hashed edge here hashes once the edge succeeds: its outputs and depfile,
    /// with the cache they are put in, if it is cached. Returns `None` if the edge is not hashed.
    fn stamp_outputs(&self, index: usize) -> Option<Arc<(Vec<String>, Option<Cache>)>> {
        let edge = self.graph.edge(self.edges[index]);
        if !edge.hashed() {
            return None;
        }
        let outputs = edge.outputs().iter().map(|&node| self.graph.path(node));
        let paths = outputs.chain(edge.depfile()).map(String::from).collect();
        let cache = self.options.cache.as_ref().filter(|_| edge.cache().is_some() && edge.pool() != Pool::Console);
        Some(Arc::new((paths, cache.cloned())))
    }
    /// Returns the inputs of a started edge, declared and discovered when it last ran, which were left on workers,
    /// with their stamps.
    fn remote_inputs(&mut self, index: usize) -> Vec<(String, Stamp)> {
        if self.left.read().is_ok_and(|left| left.is_empty()) {
            return Vec::new();
        }
        let declared = self.inputs.get(&index).into_iter().flatten();
        let mut remote: Vec<(String, Stamp)> = declared.filter(|(_, stamp)| stamp.is_remote()).cloned().collect();
        let previous = self.state.get(plan::first_output(self.graph, self.edges[index]));
        let discovered: Vec<String> = previous.into_iter().flat_map(|previous| &previous.discovered).map(|(path, _)| {
            path.clone()
        }).collect();
        for path in discovered {
            let stamp = self.stamps.path(&path);
            if stamp.is_remote() && !remote.iter().any(|(remote, _)| *remote == path) {
                remote.push((path, stamp));
            }
        }
        remote
    }
    /// Returns the outputs of an edge to leave on the worker which runs it. With [`Download::Targets`] those are its
    /// outputs which are not targets of the build, and otherwise none.
    fn leaves(&self, index: usize) -> Vec<String> {
        if self.options.download != Download::Targets {
            return Vec::new();
        }
        let edge = self.graph.edge(self.edges[index]);
        let outputs = edge.outputs().iter().filter(|node| !self.wanted.contains(node));
        outputs.map(|&node| self.graph.path(node).to_string()).collect()
    }
    /// Record that an output is left on a worker, with its stamp, or with `None`, that it is not, or no longer is.
    fn set_remote(&mut self, path: &str, stamp: Option<Stamp>) {
        self.stamps.set_remote(path, stamp);
        if let Ok(mut left) = self.left.write() {
            match stamp {
                Some(stamp) => left.insert(path.to_string(), stamp),
                None => left.remove(path),
            };
        }
    }
    /// Handle an output which was left on a worker that no longer has it. Its edge is recorded as missing the output,
    /// so that the next pass of the build builds it again.
    fn lose(&mut self, path: &str) {
        self.set_remote(path, None);
        self.lost.push(path.to_string());
        let Some(producer) = self.graph.node(path).and_then(|node| self.graph.producer(node)) else {
            return;
        };
        let Some(mut record) = self.state.get(plan::first_output(self.graph, producer)).cloned() else {
            return;
        };
        for (output, stamp) in &mut record.outputs {
            if output == path {
                *stamp = Stamp::Missing;
            }
        }
        if let Err(error) = self.state.record(record) {
            self.error.get_or_insert(error);
        }
    }
    /// Fetch the targets of the build which were left on workers. Those which no worker still has are lost.
    fn fetch_targets(&mut self) {
        let wanted: Vec<NodeId> = self.wanted.iter().copied().collect();
        let mut files = Vec::new();
        for node in wanted {
            let stamp = self.stamps.node(node);
            if stamp.is_remote() {
                files.push((self.graph.path(node).to_string(), stamp));
            }
        }
        for path in self.fetcher.fetch_all(&files, self.options.jobs.max(1)) {
            self.lose(&path);
        }
    }
    /// Update the record of each edge which left an output that has now been fetched: the output is recorded with the
    /// hash it was left with and the time and size of the fetched file, so that later builds need not hash it again.
    fn record_fetched(&mut self) {
        let mut records: HashMap<&str, crate::state::Record> = HashMap::new();
        for path in self.fetcher.fetched() {
            let Some(edge) = self.graph.node(&path).and_then(|node| self.graph.producer(node)) else {
                continue;
            };
            let key = plan::first_output(self.graph, edge);
            let record = match records.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => match self.state.get(key) {
                    Some(record) => entry.insert(record.clone()),
                    None => continue,
                },
            };
            // unless its edge ran here since, which recorded it afresh
            let Some((_, stamp)) = record.outputs.iter_mut().find(|(output, _)| *output == path) else {
                continue;
            };
            let Stamp::Remote { hash, .. } = *stamp else {
                continue;
            };
            if let Stamp::File { mtime, size } = Stamp::of(&self.options.dir.join(&path)) {
                *stamp = Stamp::Content { mtime, size, hash };
            }
        }
        for (key, record) in records {
            if self.state.get(key) == Some(&record) {
                continue;
            }
            if let Err(error) = self.state.record(record) {
                self.error.get_or_insert(error);
            }
        }
    }
    /// Returns the early outputs of an edge, by their paths, if it has any.
    fn early_outputs(&self, index: usize) -> Option<Arc<Vec<(String, NodeId)>>> {
        let early = self.graph.edge(self.edges[index]).early();
        let paths = || early.iter().map(|&node| (self.graph.path(node).to_string(), node)).collect();
        (!early.is_empty()).then(|| Arc::new(paths()))
    }
    /// Handle an edge reporting an early output. The output is stamped afresh, and once every early output has been
    /// reported, the edges which need only those stop waiting for the edge.
    fn early(&mut self, index: usize, node: NodeId, left: Option<Stamp>) {
        if left.is_some() {
            self.set_remote(self.graph.path(node), left);
        }
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
    fn action(&self, index: usize) -> &Exec {
        match self.graph.edge(self.edges[index]).kind() {
            EdgeKind::Command(action) => action,
            // the plan has no aliases
            EdgeKind::Alias => unreachable!("an alias is not run"),
        }
    }
    /// Returns the action to run for an edge: its own, with what [`Options::given`] gives it, if it runs always.
    fn given_action(&self, index: usize) -> Exec {
        let mut action = self.action(index).clone();
        let edge = self.graph.edge(self.edges[index]);
        let given = edge.always().then(|| self.options.given.get(plan::first_output(self.graph, self.edges[index])));
        if let Some(Given { args, env, .. }) = given.flatten() {
            action.args.extend(args.iter().cloned());
            action.env.retain(|(name, _)| !env.iter().any(|(given, _)| given == name));
            action.env.extend(env.iter().cloned());
        }
        action
    }
    /// Whether an edge which would run only as an input is built again is up to date after all, as a restat or hashed
    /// edge it depends on left its outputs alone, or as it is hashed and its inputs' contents are as they were.
    ///
    /// It is checked again once the edges it depends on have run, if one of them left its outputs alone, by the inputs
    /// which make it run, so that another which it depends on only through what it is built after may have changed.
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
        // create the directories of the outputs and the depfile, as the command may fail without them
        let outputs = data.outputs().iter().map(|&node| self.graph.path(node));
        for path in outputs.chain(data.depfile()) {
            // each directory is created once, as many outputs share one
            if let Some(dir) = Path::new(path).parent().filter(|dir| !dir.as_os_str().is_empty())
                && self.directories.insert(dir.to_path_buf())
            {
                let _ = std::fs::create_dir_all(self.options.dir.join(dir));
            }
        }
        // Stamp the outputs of a restat or hashed edge before it runs, to tell afterwards whether it changed them. A
        // hashed edge's are stamped by their contents, reusing the hashes of its last record.
        if data.restat() || data.hashed() {
            let previous = self.state.get(plan::first_output(self.graph, edge));
            let outputs = data.outputs().iter();
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
        if data.pool() == Pool::Serial {
            self.serial = true;
        }
        // a console edge is in rig's own process group, which cannot be killed alone
        let given = data.always().then(|| self.options.given.get(plan::first_output(self.graph, edge))).flatten();
        let timeout = given.and_then(|given| given.timeout).or(data.timeout());
        if let Some(timeout) = timeout.filter(|_| data.pool() != Pool::Console) {
            self.timeouts.insert(index, timeout);
        }
        self.status[index] = Status::Running;
        self.progress.started += 1;
        self.progress.running += 1;
        observer.started(self.graph, edge, &self.reasons[index], self.progress);
    }
    fn finish(&mut self, finished: Finished, observer: &mut dyn Observer) {
        let Finished { index, mut outcome, output, start, mut duration, cached, remote, left, lost, stamped } =
            finished;
        for (path, stamp) in stamped {
            self.stamps.know(&path, stamp);
        }
        let edge = self.edges[index];
        let data = self.graph.edge(edge);
        self.started.remove(&index);
        if data.pool() == Pool::Serial {
            self.serial = false;
        }
        let sent_to = self.sent_to.remove(&index);
        // an edge whose input was lost did not run: it runs in the next pass, after the input is built again
        if !lost.is_empty() {
            self.progress.running -= 1;
            self.progress.started -= 1;
            self.status[index] = Status::Waiting;
            self.inputs.remove(&index);
            self.keys.remove(&index);
            self.before.remove(&index);
            for path in &lost {
                self.lose(path);
            }
            observer.unstarted(self.graph, edge);
            if data.pool() == Pool::Console {
                self.console = false;
                for (index, outcome, output) in std::mem::take(&mut self.held) {
                    observer.finished(self.graph, self.edges[index], &outcome, &output, self.progress);
                }
            }
            return;
        }
        // the worker has the outputs it wrote
        if let Some(worker) = sent_to.filter(|_| remote) {
            let outputs = data.outputs().iter().map(|&node| self.graph.path(node));
            self.on_workers[worker].extend(outputs.chain(data.depfile()).map(String::from));
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
            // keep how long it took when it really ran, which estimates how long it takes on a cache miss
            let previous = self.state.get(plan::first_output(self.graph, edge));
            if let Some(previous) = previous {
                duration = Duration::from_millis(u64::from(previous.duration));
            }
        }
        if outcome.succeeded() {
            // an output left on a worker has the stamp it was left with, and any other is stamped from its file
            let outputs = data.outputs().iter().map(|&node| self.graph.path(node));
            for path in outputs {
                let stamp = left.iter().find(|(left, _)| left == path).map(|&(_, stamp)| stamp);
                if stamp.is_some() || self.left.read().is_ok_and(|left| left.contains_key(path)) {
                    self.set_remote(path, stamp);
                }
            }
            let discovered = match data.depfile() {
                Some(path) => read_depfile(&self.options.dir.join(path)),
                None => Ok(depfile::Discovered::default()),
            };
            match discovered {
                Ok(discovered) => {
                    let record = plan::record(self.graph, edge, inputs, &discovered, &mut self.stamps, start, duration);
                    if let Some(key) = key.filter(|_| !cached) {
                        // the cache only saves time, so failing to add an edge to it is not a failure of the build
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
    /// Returns where to look for the result of a cached edge: under its first key, which is also where its result is
    /// added if it runs. The thread which runs the edge does the lookup, trying each set of inputs the edge has
    /// discovered under that key.
    fn lookup(&mut self, index: usize) -> Option<Lookup> {
        let cache = self.options.cache.as_ref()?;
        let edge = self.graph.edge(self.edges[index]);
        let identity = edge.cache()?;
        if edge.pool() == Pool::Console {
            return None;
        }
        let outputs = edge.outputs().iter();
        let outputs: Vec<&str> = outputs.map(|&node| self.graph.path(node)).collect();
        let inputs = self.inputs.get(&index)?;
        let inputs: Vec<(&str, Option<&Hash>)> = inputs.iter().map(|(path, stamp)| (path.as_str(), stamp.hash())).collect();
        let action = plan::relocated_action_bytes(edge, &self.relocation);
        let values: Vec<_> = edge.pass_env().iter().map(|name| self.stamps.env_hash(name)).collect();
        let passed: Vec<_> = edge.pass_env().iter().zip(&values).map(|(name, hash)| (name.as_str(), hash.as_ref())).collect();
        let key = cache::first_key(identity, &action, &self.options.env, &passed, &outputs, edge.depfile(), &inputs);
        self.keys.insert(index, key);
        Some(Lookup { cache: cache.clone(), key, env: Arc::clone(&self.env), left: Arc::clone(&self.left) })
    }
    /// Add the result of a cached edge which has run to the cache: its outputs, its depfile and what its command
    /// printed. The inputs it discovered are added to the manifest of its first key, and the result is stored under
    /// its second key, which is made from the contents of those inputs.
    ///
    /// The edge is not added if any of its outputs is not a file it wrote.
    fn store(&mut self, index: usize, key: &Hash, record: &crate::state::Record, printed: &[u8]) -> std::io::Result<()> {
        let Some(cache) = &self.options.cache else {
            return Ok(());
        };
        let edge = self.graph.edge(self.edges[index]);
        // an output left on a worker is not here to add
        if record.outputs.iter().any(|(_, stamp)| stamp.is_remote()) {
            return Ok(());
        }
        let mut outputs = Vec::new();
        for (path, stamp) in &record.outputs {
            let Some(hash) = stamp.hash() else {
                return Ok(());
            };
            let full = self.options.dir.join(path);
            let executable = sys::is_executable(&std::fs::metadata(&full)?);
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
        // and add it to the remote cache, if that takes pushes, which also only saves time
        let _ = cache.push(key, &second, &discovery);
        Ok(())
    }
    /// Mark an edge as done, making ready the edges which were waiting only for it.
    fn done(&mut self, index: usize, changed: bool) {
        self.status[index] = Status::Done { changed };
        for dependent in self.dependents[index].clone() {
            // a dependent which already started, on the edge's early outputs, is not waiting for it
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
    let inputs = data.inputs().iter().chain(data.dependencies());
    let producers: Vec<EdgeId> = inputs.filter_map(|&node| graph.producer(node)).collect();
    producers.into_iter().any(|producer| changed(graph, producer, index, status, seen))
}

/// Returns what a depfile lists. A missing depfile lists nothing, as a command need not write one.
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
    /// Set for a command which prints rustc's JSON: called with the path of each artifact as the command reports it.
    /// The command's output is then kept as what rustc would have printed without JSON.
    pub(crate) reports: Option<&'a dyn Fn(&str)>,
}

/// Returns the path to start an action's program by. A bare name is returned as it is, to be found in `PATH`.
///
/// A relative path is relative to the action's working directory, `working`. Unix resolves it from the directory the
/// program starts in, so it is returned as it is. Windows, selected by `windows`, resolves it from rig's own
/// directory, so there it is joined to `working`.
fn program_from(program: &str, working: &Path, windows: bool) -> std::path::PathBuf {
    let path = Path::new(program);
    if windows && path.is_relative() && path.components().count() > 1 { working.join(path) } else { path.to_path_buf() }
}

/// Run an action, and return how it finished and its output, stdout and stderr together. `spawned` is called with
/// the id of its process once it has started.
///
/// The program runs directly, without a shell, in its working directory, which is relative to `dir`. It is given the
/// jobserver's `MAKEFLAGS` if there is one. If the action names a file for stderr, stderr is written there instead of
/// being captured.
///
/// A console command has the terminal, so none of its output is captured. It stays in rig's process group, so that it
/// receives what the terminal sends, such as `SIGINT`. Any other command has no stdin, and leads a process group of
/// its own, which the build signals when it is interrupted.
pub(crate) fn run_command(
    exec: &Exec,
    dir: &Path,
    console: bool,
    environment: &Environment,
    spawned: &dyn Fn(u32),
) -> (Outcome, Vec<u8>) {
    let working = exec.dir.as_ref().map_or_else(|| dir.to_path_buf(), |working| dir.join(working));
    let mut command = Command::new(program_from(&exec.program, &working, cfg!(windows)));
    command.args(&exec.args).current_dir(working);
    let (program, stderr_file) = (exec.program.as_str(), exec.stderr.as_ref().map(|path| dir.join(path)));
    if let Some(inherit) = environment.inherit {
        command.env_clear();
        for name in inherit {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    command.envs(environment.env.iter().map(|(name, value)| (name, value)));
    command.envs(exec.env.iter().map(|(name, value)| (name, value)));
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
    command.stdin(Stdio::null()).stdout(stdout);
    sys::new_group(&mut command);
    // close our copies of the pipe's write end: the output only ends once every writer is closed
    match stderr_file {
        Some(file) => {
            drop(stderr);
            command.stderr(file)
        },
        None => command.stderr(stderr),
    };
    let child = command.spawn();
    // drop the command, which still holds the pipe's write ends, for the same reason
    drop(command);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return failed(error),
    };
    sys::group_started(child.id());
    spawned(child.id());
    let mut output = Vec::new();
    let read = match environment.reports {
        None => reader.read_to_end(&mut output),
        // read each line as it is written, so that an artifact is passed on as soon as it is reported
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
    let waited = child.wait();
    sys::group_ended(child.id());
    let outcome = match (waited, read) {
        (Ok(status), Ok(_)) if status.success() => Outcome::Succeeded,
        (Ok(status), Ok(_)) => Outcome::Failed(status),
        (Err(error), _) | (_, Err(error)) => Outcome::Error(format!("failed to wait for `{program}`: {error}")),
    };
    (outcome, output)
}

/// An [`Observer`] which shows a build on stdout or a terminal.
///
/// # Line by line
///
/// By default each edge is shown on a line of its own, for output which cannot be redrawn: its status and description
/// when it finishes, followed by its output. A console edge is shown when it starts instead, as it then has the
/// terminal. A failed edge is shown with its outputs and command. With `quiet`, only the output of commands and
/// failures are shown. With `verbose`, each edge is shown by its command rather than its description.
///
/// Without `colour`, as when the output is not a terminal, the escape sequences which colour what commands printed
/// are removed. Commands can therefore always be told to print in colour, as rustc is with `--color=always`, so that
/// their actions do not depend on where the build's output goes.
///
/// # In place
///
/// [`Self::in_place`] shows the build on a terminal, with one line for each edge running at once, redrawn in place
/// with escape sequences.
///
/// - A running edge is shown with the order it started in and how long it has run, as
///   `[ 2/15] RUSTC liba.rlib (3m)`.
/// - A line whose edge has finished, and which no other edge has taken, is shown as `[  /15] -`.
/// - Each line is cut to the terminal's width, so that none wraps.
/// - A finished edge leaves no line behind, but what it printed, and its failure, are shown above the lines.
/// - The cursor is left at the end of the last line, so that there is no blank line below.
/// - The lines are removed while a console edge has the terminal.
/// - When the build is done, the lines are replaced by how many of the edges finished and how long the build took,
///   as `[15/15] completed in 3m 14s`, or `[12/15] stopped after 2s` if some did not finish.
pub struct Console<W: std::io::Write> {
    pub out: W,
    pub quiet: bool,
    pub verbose: bool,
    pub colour: bool,
    /// The width of the terminal, if what is running is shown in place.
    width: Option<usize>,
    /// The lines, as many as the most edges which have run at once. Each holds the edge running in it, if any.
    lines: Vec<Option<Running>>,
    /// The number of edges which may run, as last reported.
    total: usize,
    /// The lines as last drawn. The cursor is at the end of the last one.
    drawn: Vec<String>,
    /// Whether a console edge has the terminal.
    terminal_held: bool,
    /// How far the build had got when an edge last finished, if one has.
    progress: Option<Progress>,
    /// When the console was made, which is taken as when the build started.
    began: Instant,
}

/// An edge which is running, shown in place.
struct Running {
    edge: EdgeId,
    /// The order it started in, from one.
    number: usize,
    description: String,
    started: Instant,
}

impl<W: std::io::Write> Console<W> {
    #[must_use]
    pub fn new(out: W, quiet: bool, verbose: bool, colour: bool) -> Self {
        let (lines, drawn, began) = (Vec::new(), Vec::new(), Instant::now());
        Self { out, quiet, verbose, colour, width: None, lines, total: 0, drawn, terminal_held: false, progress: None,
            began }
    }
    /// Show what is running in place, on a terminal this wide, unless `quiet`.
    #[must_use]
    pub fn in_place(mut self, width: usize) -> Self {
        self.width = (!self.quiet && width > 0).then_some(width);
        self
    }
    fn description(&self, graph: &Graph, edge: EdgeId) -> String {
        let data = graph.edge(edge);
        match (data.description(), data.kind()) {
            (description, EdgeKind::Command(action)) if description.is_empty() || self.verbose => action.command_line(),
            (description, _) => description.to_string(),
        }
    }
    fn status(&mut self, graph: &Graph, edge: EdgeId, progress: Progress) {
        if self.quiet {
            return;
        }
        let description = self.description(graph, edge);
        let _ = writeln!(self.out, "[{}/{}] {description}", progress.finished, progress.total);
    }
    /// Erase the lines, leaving the cursor where they started.
    fn clear(&mut self) {
        if self.drawn.is_empty() {
            return;
        }
        let _ = write!(self.out, "\r");
        if self.drawn.len() > 1 {
            let _ = write!(self.out, "\x1b[{}A", self.drawn.len() - 1);
        }
        let _ = write!(self.out, "\x1b[J");
        self.drawn.clear();
    }
    /// Returns the lines as they are now.
    fn render(&self, width: usize) -> Vec<String> {
        let digits = self.total.to_string().len();
        let total = self.total;
        let line = |running: &Option<Running>| {
            let Some(Running { number, description, started, .. }) = running else {
                return format!("[{:digits$}/{total}] -", "");
            };
            let (head, tail) = (format!("[{number:>digits$}/{total}] "), format!(" ({})", elapsed(started.elapsed())));
            // Cut the description short so that the line does not wrap, which would put the cursor a line lower
            // than expected. How long the edge has run is kept.
            let room = width.saturating_sub(1 + head.chars().count() + tail.chars().count());
            let description: String = description.chars().take(room).collect();
            let line = format!("{head}{}{tail}", description.trim_end());
            line.chars().take(width.saturating_sub(1)).collect()
        };
        self.lines.iter().map(line).collect()
    }
    /// Draw the lines again, if they have changed, unless a console edge has the terminal.
    fn draw(&mut self) {
        let Some(width) = self.width.filter(|_| !self.terminal_held) else {
            return;
        };
        let lines = self.render(width);
        if lines == self.drawn {
            return;
        }
        self.clear();
        let _ = write!(self.out, "{}", lines.join("\n"));
        self.drawn = lines;
        let _ = self.out.flush();
    }
    /// Free the line of an edge which is no longer running.
    fn free(&mut self, edge: EdgeId) {
        for line in &mut self.lines {
            if line.as_ref().is_some_and(|running| running.edge == edge) {
                *line = None;
            }
        }
    }
}

/// Returns how long a build took, in hours, minutes and seconds, as `3m 14s`, or in seconds to a tenth below a minute.
fn total_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{:.1}s", duration.as_secs_f64()),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m {}s", seconds / 3600, seconds / 60 % 60, seconds % 60),
    }
}

/// Returns how long an edge has run, in whole seconds, minutes or hours, whichever is the largest it has reached.
fn elapsed(duration: Duration) -> String {
    match duration.as_secs() {
        seconds @ 0..60 => format!("{seconds}s"),
        seconds @ 60..3600 => format!("{}m", seconds / 60),
        seconds => format!("{}h", seconds / 3600),
    }
}

impl<W: std::io::Write> Observer for Console<W> {
    fn started(&mut self, graph: &Graph, edge: EdgeId, _: &Reason, progress: Progress) {
        self.total = progress.total;
        if graph.edge(edge).pool() == Pool::Console {
            // a console edge has the terminal to itself, so the lines are erased
            self.clear();
            self.terminal_held = self.width.is_some();
            self.status(graph, edge, progress);
            let _ = self.out.flush();
        } else if self.width.is_some() {
            self.free(edge);
            let description = self.description(graph, edge);
            let running = Running { edge, number: progress.started, description, started: Instant::now() };
            // in the first line which is free
            match self.lines.iter_mut().find(|line| line.is_none()) {
                Some(line) => *line = Some(running),
                None => self.lines.push(Some(running)),
            }
            self.draw();
        }
    }
    fn unstarted(&mut self, _: &Graph, edge: EdgeId) {
        self.free(edge);
        self.draw();
    }
    fn tick(&mut self) {
        self.draw();
    }
    fn done(&mut self) {
        if self.width.is_none() {
            return;
        }
        self.clear();
        self.lines.clear();
        if let Some(Progress { finished, total, .. }) = self.progress.take() {
            let took = total_time(self.began.elapsed());
            let ended = if finished == total { "completed in" } else { "stopped after" };
            let _ = writeln!(self.out, "[{finished}/{total}] {ended} {took}");
        }
        let _ = self.out.flush();
    }
    fn finished(&mut self, graph: &Graph, edge: EdgeId, outcome: &Outcome, output: &[u8], progress: Progress) {
        let data = graph.edge(edge);
        let failed = !outcome.succeeded() && !matches!(outcome, Outcome::Interrupted);
        self.total = progress.total;
        if self.width.is_some() {
            // show what it printed above the lines, then draw the lines again below, without the finished edge
            self.free(edge);
            if failed || !output.is_empty() || data.pool() == Pool::Console {
                self.clear();
            }
            if data.pool() == Pool::Console {
                self.terminal_held = false;
            }
            self.progress = Some(progress);
        } else if data.pool() != Pool::Console {
            self.status(graph, edge, progress);
        }
        if failed {
            let outputs = data.outputs().iter();
            let outputs: Vec<&str> = outputs.map(|&node| graph.path(node)).collect();
            let _ = writeln!(self.out, "FAILED: {}", outputs.join(" "));
            if let EdgeKind::Command(action) = data.kind() {
                let _ = writeln!(self.out, "{}", action.command_line());
            }
            match outcome {
                Outcome::Error(error) => _ = writeln!(self.out, "{error}"),
                Outcome::TimedOut(timeout) => _ = writeln!(self.out, "timed out after {}", total_time(*timeout)),
                _ => (),
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
        self.draw();
        let _ = self.out.flush();
    }
}

/// Returns the width in columns of the terminal a file descriptor refers to, or `None` if it is not a terminal.
#[must_use]
pub fn terminal_width(fd: i32) -> Option<usize> {
    sys::terminal_width(fd)
}

/// Returns what a command printed without its escape sequences for colour, `ESC [ ... <letter>`, and for choosing
/// the character set, `ESC ( <byte>`, which rustc writes when it resets colour.
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
        let graph = graph().rule("say", &[("command", "printf '\\e[1mbold\\e[0m\\n' && touch $out")]);
        let mut build = Build::new("colour", graph.edge("say", &["a"], &[]).finish(), &[]);
        let mut console = Console::new(Vec::new(), true, false, false);
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
        /// Record each edge of `quick` as having taken at most a quarter of the time `slow` took.
        ///
        /// The tests of workers expect `slow` to come first on the critical path. Without this, the time the test
        /// helper takes to start could make a quick edge look slower: under Wine on CI's runners, starting it has
        /// taken longer than the second `slow` runs for.
        fn quicker_than(&mut self, slow: &str, quick: &[&str]) {
            let most = self.state.get(slow).expect("the slow edge has run").duration / 4;
            for output in quick {
                let mut record = self.state.get(output).expect("the quick edge has run").clone();
                record.duration = record.duration.min(most);
                self.state.record(record).unwrap();
            }
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
    fn shows_what_runs_in_place() {
        let graph = cat()
            .edge("cat", &["ab"], &["a", "b"])
            .edge("cat", &["cd"], &["c", "d"])
            .edge("cat", &["ef"], &["e", "f"])
            .finish();
        let edge = |output: &str| graph.producer(graph.node(output).unwrap()).unwrap();
        let (ab, cd, ef) = (edge("ab"), edge("cd"), edge("ef"));
        let mut console = Console::new(Vec::new(), false, false, true).in_place(80);
        let shown = |console: &mut Console<Vec<u8>>| String::from_utf8(std::mem::take(&mut console.out)).unwrap();
        let progress = |started, finished| Progress { total: 12, started, finished, running: 0 };
        console.started(&graph, ab, &Reason::NoRecord, progress(1, 0));
        assert_eq!(shown(&mut console), "[ 1/12] cat a b > ab (0s)");
        console.started(&graph, cd, &Reason::NoRecord, progress(2, 0));
        // the cursor is left at the end of the last line, so that there is no blank line below them
        assert_eq!(shown(&mut console), "\r\x1b[J[ 1/12] cat a b > ab (0s)\n[ 2/12] cat c d > cd (0s)");
        // nothing is drawn while it is the same
        console.tick();
        assert_eq!(shown(&mut console), "");
        // what finishes is not kept, though what it printed is, above the lines, where its line is free
        console.finished(&graph, ab, &Outcome::Succeeded, b"printed", progress(2, 1));
        assert_eq!(shown(&mut console), "\r\x1b[1A\x1b[Jprinted\n[  /12] -\n[ 2/12] cat c d > cd (0s)");
        // until another takes it
        console.started(&graph, ef, &Reason::NoRecord, progress(3, 1));
        assert_eq!(shown(&mut console), "\r\x1b[1A\x1b[J[ 3/12] cat e f > ef (0s)\n[ 2/12] cat c d > cd (0s)");
        console.finished(&graph, ef, &Outcome::Succeeded, b"", progress(3, 2));
        assert_eq!(shown(&mut console), "\r\x1b[1A\x1b[J[  /12] -\n[ 2/12] cat c d > cd (0s)");
        // and once the build is done, the lines are replaced by how far it got, and how long it took
        console.finished(&graph, cd, &Outcome::Succeeded, b"", progress(3, 3));
        shown(&mut console);
        console.done();
        let summary = shown(&mut console);
        let stopped = summary.strip_prefix("\r\x1b[1A\x1b[J[3/12] stopped after ");
        let stopped = stopped.and_then(|rest| rest.strip_suffix("s\n"));
        assert!(stopped.is_some_and(|took| took.parse::<f64>().is_ok()), "{summary:?}");
        let mut console = Console::new(Vec::new(), false, false, true).in_place(80);
        let all = Progress { total: 1, started: 1, finished: 1, running: 0 };
        console.started(&graph, ab, &Reason::NoRecord, all);
        console.finished(&graph, ab, &Outcome::Succeeded, b"", all);
        shown(&mut console);
        console.done();
        assert!(shown(&mut console).starts_with("\r\x1b[J[1/1] completed in "));
        assert_eq!(total_time(Duration::from_millis(3440)), "3.4s");
        assert_eq!(total_time(Duration::from_secs(194)), "3m 14s");
        assert_eq!(total_time(Duration::from_secs(3725)), "1h 2m 5s");
        // a line is cut to the terminal's width, keeping how long it has run
        let mut console = Console::new(Vec::new(), false, false, true).in_place(20);
        console.started(&graph, ab, &Reason::NoRecord, progress(1, 0));
        assert_eq!(shown(&mut console), "[ 1/12] cat a (0s)");
        // which long running edges show in minutes and hours
        assert_eq!([elapsed(Duration::from_secs(59)), elapsed(Duration::from_secs(185))], ["59s", "3m"]);
        assert_eq!(elapsed(Duration::from_secs(7300)), "2h");
        // while quiet, nothing is shown in place
        let mut console = Console::new(Vec::new(), true, false, true).in_place(80);
        console.started(&graph, ab, &Reason::NoRecord, progress(1, 0));
        assert_eq!(shown(&mut console), "");
    }

    #[test]
    fn runs_programs_and_captures_what_they_print() {
        let graph = graph().rule("echo", &[("command", "echo built > $out && echo printed")]);
        let graph = graph.edge("echo", &["out"], &[]);
        let mut build = Build::new("printed", graph.finish(), &[]);
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(build.read("out"), "built\n");
        assert_eq!(finished[0].1, "printed\n");
        assert_eq!(build.ran(&[]), Vec::<String>::new());
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
        // Each edge marks that it has started, then waits for the others to do the same. That only succeeds if they
        // run at once: run one after the other, the first would wait for a second which has not started. Unlike a
        // timing check, it holds however long the test helper takes to start, which is slow on Windows on Arm.
        let wait = "mkdir $out.started && wait-for a.started 10 && wait-for b.started 10 && wait-for c.started 10 && touch $out";
        let graph = graph().rule("wait", &[("command", wait)])
            .edge("wait", &["a"], &[]).edge("wait", &["b"], &[]).edge("wait", &["c"], &[]);
        let mut build = Build::new("parallel", graph.finish(), &[]);
        build.options.jobs = 3;
        assert_eq!(build.ran(&[]).len(), 3);
    }

    /// What a build tells its observer, in order: each edge which starts or finishes, by its first output, with how
    /// many edges were running then, and how each finished.
    #[derive(Default)]
    struct Events {
        events: Vec<(&'static str, String, usize)>,
        outcomes: Vec<(String, String)>,
    }
    impl Observer for Events {
        fn started(&mut self, graph: &Graph, edge: EdgeId, _: &Reason, progress: Progress) {
            let output = graph.path(graph.edge(edge).outputs()[0]).to_string();
            self.events.push(("started", output, progress.running));
        }
        fn finished(&mut self, graph: &Graph, edge: EdgeId, outcome: &Outcome, _: &[u8], progress: Progress) {
            let output = graph.path(graph.edge(edge).outputs()[0]).to_string();
            self.events.push(("finished", output.clone(), progress.running));
            self.outcomes.push((output, format!("{outcome:?}")));
        }
    }

    #[test]
    fn runs_what_runs_always_every_time() {
        // an edge which runs always runs on every build, while the edge it reads is up to date
        let graph = cat().rule("test", &[("command", "cat $in"), ("always", "")]);
        let graph = graph.edge("cat", &["copy"], &["source"]).edge("test", &["checked"], &["copy"]).finish();
        let mut build = Build::new("always", graph, &[("source", "a")]);
        assert_eq!(build.ran(&[]), ["copy", "checked"]);
        assert_eq!(build.ran(&[]), ["checked"]);
        // and why, as a dry run shows it
        let mut stamps = Stamps::new(&build.graph, &build.dir);
        let planned = plan::plan(&build.graph, &build.state, &mut stamps, &[]).unwrap();
        assert_eq!(planned.edges.iter().map(|(_, reason)| reason).collect::<Vec<_>>(), [&Reason::Always]);
        // a cache does not keep it, whatever `cache` says
        let cache = TempCache::new("always");
        let mut graph = cat().rule("test", &[("command", "cat $in"), ("always", "")]).cached("toolchain");
        graph = graph.edge("test", &["checked"], &["source"]);
        let mut build = Build::new("always-cached", graph.finish(), &[("source", "a")]);
        build.options.cache = Some(cache.0.clone());
        assert_eq!(build.ran(&[]), ["checked"]);
        assert_eq!(build.ran(&[]), ["checked"]);
        assert_eq!(cache.0.stats().results, 0);
    }

    #[test]
    fn kills_what_runs_longer_than_its_timeout() {
        // The edge starts a process which sleeps for a minute, and waits for it, so the build only finishes soon if
        // that process is killed too, as the edge's process group is.
        let stuck = graph().rule("stuck", &[("command", "echo started && spawn 60 child.pid"), ("timeout", "0.5")]);
        let mut build = Build::new("timeout", stuck.edge("stuck", &["out"], &[]).finish(), &[]);
        let started = Instant::now();
        let mut events = Events::default();
        let report = super::build(&build.graph, &mut build.state, &build.options, &[], &mut events).unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
        assert_eq!((report.failed, report.succeeded()), (1, false));
        assert_eq!(events.outcomes, [("out".to_string(), "TimedOut(500ms)".to_string())]);
        // which the console shows
        let mut console = Console::new(Vec::new(), false, false, false);
        console.finished(&build.graph, EdgeId(0), &Outcome::TimedOut(Duration::from_millis(500)), b"", Progress::default());
        let shown = String::from_utf8(console.out).unwrap();
        assert!(shown.contains("\ntimed out after 0.5s\n"), "{shown}");
        // a timeout given to an edge which runs always replaces its own
        let stuck = graph().rule("stuck", &[("command", "sleep 60"), ("always", ""), ("timeout", "60")]);
        build.graph = stuck.edge("stuck", &["out"], &[]).finish();
        let given = Given { timeout: Some(Duration::from_millis(300)), ..Given::default() };
        build.options.given.insert("out".into(), given);
        let started = Instant::now();
        let mut events = Events::default();
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut events).unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
        assert_eq!(events.outcomes, [("out".to_string(), "TimedOut(300ms)".to_string())]);
    }

    #[test]
    fn gives_what_runs_always_its_arguments() {
        // the arguments and variables given to an edge which runs always follow its own, and replace its variables
        let steps = ["touch", "out", "&&", "getenv", "SET", "&&", "getenv", "GIVEN", "&&", "args", "own"];
        let mut exec = crate::test_helper::exec(steps.into_iter().map(String::from).collect());
        exec.env.push(("SET".into(), "by the edge".into()));
        let mut always = Graph::new();
        let out = always.file("out");
        let kind = EdgeKind::Command(exec.clone());
        always.add_edge(crate::Edge { kind, outputs: vec![out], always: true, ..Default::default() }).unwrap();
        let mut build = Build::new("given", always, &[]);
        let env = vec![("SET".into(), "replaced".into()), ("GIVEN".into(), "given".into())];
        build.options.given.insert("out".into(), Given { args: vec!["extra".into()], env, timeout: None });
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(finished, [("out".to_string(), "replaced\ngiven\nown|extra|".to_string())]);
        // while an edge which does not run always is not given them, as they are not part of its record
        build.graph = program(exec);
        std::fs::remove_file(build.dir.join("out")).unwrap();
        let (_, finished) = build.build(&[]);
        assert_eq!(finished, [("out".to_string(), "by the edge\nunset\nown|".to_string())]);
    }

    #[test]
    fn runs_what_is_serial_alone() {
        // nothing else runs while the serial edge does, however many jobs there are, which the order in which the
        // edges start and finish shows, whatever their times
        let graph = graph().rule("quick", &[("command", "sleep 0.2 && touch $out")]);
        let graph = graph.rule("serial", &[("command", "sleep 0.2 && touch $out"), ("pool", "serial")]);
        let graph = graph.edge("quick", &["a"], &[]).edge("serial", &["s"], &[]).edge("quick", &["b"], &[]);
        let graph = graph.edge("quick", &["c"], &[]).finish();
        let mut build = Build::new("serial", graph, &[]);
        build.options.jobs = 4;
        let mut events = Events::default();
        let report = super::build(&build.graph, &mut build.state, &build.options, &[], &mut events).unwrap();
        assert!(report.succeeded(), "{:?}", events.events);
        let started = events.events.iter().position(|(event, output, _)| *event == "started" && output == "s").unwrap();
        let next = &events.events[started + 1];
        // it started with nothing else running, and finished before anything else started or finished
        assert_eq!(events.events[started].2, 1, "{:?}", events.events);
        assert_eq!((next.0, next.1.as_str()), ("finished", "s"), "{:?}", events.events);
        // while the others ran at once
        assert!(events.events.iter().any(|&(_, _, running)| running > 1), "{:?}", events.events);
    }

    /// A graph of one edge, `out`, which runs a program directly.
    fn program(exec: Exec) -> Graph {
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(exec);
        graph.add_edge(crate::Edge { kind, outputs: vec![out], ..Default::default() }).unwrap();
        graph
    }

    #[test]
    fn runs_programs_without_a_shell() {
        // the arguments reach the program as they are, as no shell splits or expands them, in its working directory,
        // with its variables, and its stderr in a file
        let steps = ["args", "a b", "$HOME", "it's", ">", "../out", "&&", "getenv", "VALUE", ">>", "../out", "&&"];
        let steps = steps.into_iter().chain(["eecho", "to-stderr"]).map(String::from).collect();
        let mut exec = crate::test_helper::exec(steps);
        exec.env.push(("VALUE".into(), "v'1 2".into()));
        exec.dir = Some("sub".into());
        exec.stderr = Some("err.txt".into());
        let mut build = Build::new("exec", program(exec), &[]);
        std::fs::create_dir_all(build.dir.join("sub")).unwrap();
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!(build.read("out"), "a b|$HOME|it's|v'1 2\n");
        assert_eq!(build.read("err.txt"), "to-stderr\n");
        // with nothing captured from stderr
        assert_eq!(finished, [("out".to_string(), String::new())]);
        assert_eq!(build.build(&[]).1, Vec::<(String, String)>::new());
    }

    #[test]
    fn finds_a_relative_program_from_its_working_directory() {
        // a program given by a relative path is found from the action's working directory, as Unix finds it, which on
        // Windows is joined to it, as a compiler given by its path is run from the build directory, while a name is
        // found in `PATH`
        let working = Path::new("build").join("sub");
        let tool = Path::new("..").join("tools").join("cc.exe");
        assert_eq!(program_from(&tool.display().to_string(), &working, true), working.join(&tool));
        assert_eq!(program_from(&tool.display().to_string(), &working, false), tool);
        assert_eq!(program_from("cc", &working, true), Path::new("cc"));
        let absolute = std::env::temp_dir().join("cc.exe");
        assert_eq!(program_from(&absolute.display().to_string(), &working, true), absolute);
    }

    #[test]
    fn shows_a_failed_program_as_a_command_line() {
        let mut exec = crate::test_helper::exec(vec!["exit".into(), "1".into(), "a b".into(), "c".into()]);
        exec.env.push(("X".into(), "1".into()));
        let line = exec.command_line();
        let helper = Exec { program: crate::test_helper::program(), ..Exec::default() }.command_line();
        assert_eq!(line, format!("X=1 {helper} --rig-test-helper exit 1 'a b' c"));
        let mut build = Build::new("exec-failed", program(exec), &[]);
        let mut console = Console::new(Vec::new(), true, false, true);
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        assert_eq!(String::from_utf8(console.out).unwrap(), format!("FAILED: out\n{line}\n"));
        // and a program which is not found fails
        let exec = Exec { program: "rig-no-such-program".into(), ..Default::default() };
        let mut build = Build::new("exec-missing", program(exec), &[]);
        let (report, finished) = build.build(&[]);
        assert_eq!(report.failed, 1, "{finished:?}");
    }

    #[test]
    fn captures_output() {
        let graph = graph().rule("say", &[("command", "echo out && eecho err && touch $out")]);
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
                ("command", "cat $in > $out && echo $out: $in 'extra\\ file.h' > $out.d"),
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
                    "getenv RIG_TEST_VARIABLE > $out && printf '$out:\\n# env-dep:RIG_TEST_VARIABLE\\n' > $out.d",
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
                ("command", "cat $in extra.h > $out && echo $out: $in extra.h > $out.d && echo compiled"),
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
        let cc = "cat $in extra.h > $out && echo $out: $in extra.h > $out.d && echo compiled $out";
        graph()
            .rule("slow", &[("command", SLOW)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d")])
            .rule("hidden", &[
                ("command", "cat $in hidden > $out && echo $out: $in > $out.d"),
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
        build.quicker_than("slow", &["a.o", "b.o", "c.o"]);
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
    /// when it last ran, and holds the one job a build has, so that the others run on a worker, if they can: a second,
    /// which is clearly longer than sending one there takes, wherever the test helper is slow to start, as under Wine,
    /// or on Windows on Arm.
    const SLOW: &str = "sleep 1 && touch $out";

    /// The rule of the slow edge of [`copied`], which holds the one job for 0.6 s, which the times of its tests' links,
    /// workers and copies are chosen against: half a second longer than a copy of 0.1 s, so that it is the first to
    /// run however long the test helper takes to start, as on macOS, where that varies by more than 0.2 s.
    const BRIEF: &str = "sleep 0.6 && touch $out";

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
        let cc = "cat $in > $out && echo $out: $in > $out.d";
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
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
        build.quicker_than("slow", &["portable.o", "riscv.o", "gpu.o"]);
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

    /// A graph of the slow edge, by [`BRIEF`], and of two cached edges which copy their inputs, once they have slept
    /// for as many seconds as their inputs give.
    fn copied() -> Graph {
        graph()
            .rule("slow", &[("command", BRIEF)])
            .edge("slow", &["slow"], &[])
            .cached("toolchain")
            .rule("cp", &[("command", "sleep-for $in && cp $in $out")])
            .edge("cp", &["a.o"], &["a"])
            .edge("cp", &["b.o"], &["b"])
            .finish()
    }

    #[test]
    fn sends_edges_to_the_worker_which_has_their_inputs() {
        // an edge which writes a large file, which only the second worker runs, and one which reads it, which either
        // may run
        let graph = graph()
            .cached("large")
            .rule("large", &[("command", "zeros 4000000 > $out && cat $in >> $out")])
            .edge("large", &["large"], &["seed"])
            .cached("toolchain")
            .rule("count", &[("command", "size $in > $out")])
            .edge("count", &["count"], &["large"])
            .finish();
        let mut build = Build::new("has-inputs", graph, &[("seed", "s\n")]);
        assert_eq!(build.ran(&[]), ["large", "count"]);
        remove(&build, &["large", "count"]);
        let cache = TempCache::new("has-inputs-cache");
        build.options.cache = Some(cache.0.clone());
        build.options.workers_only = true;
        // over links alike, slow enough that sending the large file would take a while
        let link = crate::remote::Link { latency: Duration::ZERO, bytes_per_second: 1e6, speed: 1.0 };
        let linked = |cache: &TempCache, identities: &[&str]| {
            let worker = Arc::into_inner(worker(cache, identities)).unwrap().with_link(link);
            assert!(worker.capabilities().is_ok());
            Arc::new(worker)
        };
        let (first, second) = (TempCache::new("has-inputs-first"), TempCache::new("has-inputs-second"));
        build.options.workers = vec![linked(&first, &["toolchain"]), linked(&second, &["large", "toolchain"])];
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        assert_eq!(report.remote, 2, "{finished:?}");
        // what reads the large file runs on the worker which wrote it, rather than the first which would be sent it
        let counted = crate::blake3::hash(build.read("count").as_bytes());
        assert!(second.0.has_blob(&counted) && !first.0.has_blob(&counted));
    }

    /// A worker over a link which is assumed to be as given.
    fn linked(cache: &TempCache, link: crate::remote::Link) -> Arc<Remote> {
        let worker = Arc::into_inner(worker(cache, &["toolchain"])).unwrap().with_link(link);
        assert!(worker.capabilities().is_ok());
        Arc::new(worker)
    }

    #[test]
    fn estimates_what_has_not_run_by_what_has() {
        // edges of a toolchain, two of which have run, with the slow edge, which has too
        let cc = "cat $in > $out && echo $out: $in > $out.d";
        let graph = |all: bool| {
            let graph = graph()
                .rule("slow", &[("command", SLOW)])
                .edge("slow", &["slow"], &[])
                .cached("toolchain")
                .rule("cc", &[("command", cc), ("depfile", "$out.d"), ("discover", "echo $out: $in > $out.d")])
                .edge("cc", &["a.o"], &["a.c"])
                .edge("cc", &["b.o"], &["b.c"]);
            if all { graph.edge("cc", &["c.o"], &["c.c"]).finish() } else { graph.finish() }
        };
        let files = [("a.c", "a\n"), ("b.c", "b\n"), ("c.c", "c\n")];
        let mut build = Build::new("estimates", graph(false), &files);
        let first = TempCache::new("estimates-first");
        build.options.cache = Some(first.0.clone());
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
        build.quicker_than("slow", &["a.o", "b.o"]);
        // with one job, which the slow edge holds, the edge which has not run, which would wait for it, as it is
        // estimated to take as long as those which have, and to send as little, goes to the worker
        build.graph = graph(true);
        let store = TempCache::new("estimates-store");
        let link = crate::remote::Link { latency: Duration::from_millis(1), bytes_per_second: 1e9, speed: 1.0 };
        build.options.workers = vec![linked(&store, link)];
        remove(&build, &["slow"]);
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{finished:?}");
        assert_eq!((report.ran, report.remote), (2, 1), "{finished:?}");
        assert_eq!(build.read("c.o"), "c\n");
    }

    #[test]
    fn waits_for_its_jobs_where_sending_would_take_longer() {
        let mut build = Build::new("waits", copied(), &[("a", "0\n"), ("b", "0\n")]);
        let first = TempCache::new("waits-first");
        build.options.cache = Some(first.0.clone());
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
        // over a link with two seconds' latency, sending takes longer than the slow edge has left, or the others take
        let store = TempCache::new("waits-store");
        let slow = crate::remote::Link { latency: Duration::from_secs(2), bytes_per_second: 1e9, speed: 1.0 };
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
        // the edges which copy their inputs take 0.1 s here, while the slow edge holds the one job for 0.6 s, as it is
        // the first to run only if it took longer when it last ran, by enough that the copies, which start a process
        // more, do not overtake it on a machine which starts them slowly, as with 0.2 s and then 0.3 s on macOS, where
        // one of them took the job, and the worker ran only the other
        let mut build = Build::new("soonest", copied(), &[("a", "0.1\n"), ("b", "0.1\n")]);
        let first = TempCache::new("soonest-first");
        build.options.cache = Some(first.0.clone());
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
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
        // an edge which took 0.5 s here, which a worker runs as fast, is measured as taking as long there, as long enough
        // that what the worker takes beside running it, as to start the test helper, slowly on Windows, is a small part
        let mut build = Build::new("speed", copied(), &[("a", "0.5\n"), ("b", "0.5\n")]);
        let first = TempCache::new("speed-first");
        build.options.cache = Some(first.0.clone());
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
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
        let cc = "cat $in extra.h > $out && echo $out: $in extra.h > $out.d";
        let graph = graph()
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d"), ("discover", "echo $out: $in extra.h > $out.d")])
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
    fn leaves_what_is_not_a_target_on_workers() {
        // an edge whose output another reads on the worker, and an edge which is not cached reads here
        let cc = "cat $in > $out && echo $out: $in > $out.d";
        let graph = graph()
            .rule("cp", &[("command", "cp $in $out")])
            .edge("cp", &["here"], &["mid"])
            .cached("toolchain")
            .rule("cc", &[("command", cc), ("depfile", "$out.d")])
            .rule("twice", &[("command", "cat $in $in > $out && echo $out: $in > $out.d"), ("depfile", "$out.d")])
            .edge("cc", &["mid"], &["a.c"])
            .edge("twice", &["top"], &["mid"])
            .finish();
        let mut build = Build::new("leaves", graph, &[("a.c", "a\n")]);
        let cache = TempCache::new("leaves-cache");
        build.options.cache = Some(cache.0.clone());
        // the first build runs here, so that what the edges read is known
        assert_eq!(build.ran(&["top"]), ["mid", "top"]);
        build.write("a.c", "b\n");
        build.options.workers_only = true;
        build.options.download = Download::Targets;
        let store = TempCache::new("leaves-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        let (report, finished) = build.build(&["top"]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        assert_eq!(report.remote, 2, "{finished:?}");
        // the target is fetched, as is the depfile of what it read, which is left there
        assert_eq!(build.read("top"), "b\nb\n");
        assert_eq!(build.read("mid.d"), "mid: a.c\n");
        assert!(!build.dir.join("mid").exists());
        // what was left is kept from the worker's collection, as each build with it marks it as used
        let blob = store.0.blob_path(&crate::blake3::hash(b"b\n"));
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        std::fs::File::options().write(true).open(&blob).unwrap().set_modified(old).unwrap();
        assert_eq!(build.ran(&["top"]), Vec::<String>::new());
        assert!(std::fs::metadata(&blob).unwrap().modified().unwrap() > old);
        // once no worker has it, what reads it here has it built again first, and is left on the new worker until it
        // is fetched
        let fresh = TempCache::new("leaves-fresh");
        build.options.workers = vec![worker(&fresh, &["toolchain"])];
        let (report, finished) = build.build(&["here"]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        assert_eq!((report.lost, report.remote), (1, 1), "{finished:?}");
        let order: Vec<&str> = finished.iter().map(|(output, _)| output.as_str()).collect();
        assert_eq!(order, ["mid", "here"]);
        assert_eq!(build.read("here"), "b\n");
        // what was fetched is recorded by its contents, so that it is not hashed again
        let fetched = build.state.get("mid").unwrap().outputs[0].1;
        assert!(matches!(fetched, Stamp::Content { hash, .. } if hash == crate::blake3::hash(b"b\n")), "{fetched:?}");
        // what was fetched is the same as was left, so nothing is out of date
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("a.c", "c\n");
        // as `here` and `top` each wait only for `mid`, they finish in either order
        let mut ran = build.ran(&[]);
        ran.sort_unstable();
        assert_eq!(ran, ["here", "mid", "top"]);
        assert_eq!(build.read("top"), "c\nc\n");
        // a target which an earlier build left on a worker, which no longer has it, is built again to be fetched
        build.write("a.c", "d\n");
        assert_eq!(build.ran(&["top"]), ["mid", "top"]);
        assert!(!build.dir.join("mid").exists());
        let other = TempCache::new("leaves-other");
        build.options.workers = vec![worker(&other, &["toolchain"])];
        let (report, finished) = build.build(&["mid"]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        assert_eq!((report.lost, report.remote), (1, 1), "{finished:?}");
        assert_eq!(build.read("mid"), "d\n");
        assert_eq!(build.ran(&[]), ["here"]);
    }

    #[test]
    fn compares_paths_as_rustc_reports_them() {
        assert!(same_path("./a/b.rmeta", "a/b.rmeta"));
        assert!(!same_path("a/b.rmeta", "a/c.rmeta"));
        // with the separator jig gives rustc on Windows, which it reports the artifact by
        assert_eq!(same_path("a\\b.rmeta", "a/b.rmeta"), cfg!(windows));
    }

    #[test]
    fn starts_what_needs_early_outputs_once_they_are_written() {
        // an edge which writes its metadata, reports it as rustc does, prints a diagnostic, and goes on for a while,
        // before it writes the rest
        // with `$` escaped, as the rule's command has it
        let artifact = r#"{"$$message_type":"artifact","artifact":"a.meta","emit":"metadata"}"#;
        let diagnostic = r#"{"$$message_type":"diagnostic","message":"unused","rendered":"warning: unused\n"}"#;
        let report = |json: &str| format!("eecho '{json}'");
        let compile = format!(
            "echo meta > a.meta && {} && {} && sleep 0.5 && echo all > a.out",
            report(artifact),
            report(diagnostic),
        );
        let graph = graph()
            .rule("compile", &[("command", &compile)])
            .early(&["a.meta"])
            .edge("compile", &["a.out", "a.meta"], &[])
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
    fn starts_what_needs_early_outputs_a_worker_wrote() {
        // as above, but with a while longer to write the rest, run on a worker
        let artifact = r#"{"$$message_type":"artifact","artifact":"a.meta","emit":"metadata"}"#;
        let compile = format!("echo meta > a.meta && eecho '{artifact}' && sleep 2 && echo all > a.out");
        let graph = graph()
            .cached("toolchain")
            .rule("compile", &[("command", &compile)])
            .early(&["a.meta"])
            .edge("compile", &["a.out", "a.meta"], &[])
            .rule("cp", &[("command", "cp $in $out")])
            .edge("cp", &["b.out"], &["a.meta"])
            .finish();
        let mut build = Build::new("early-worker", graph, &[]);
        build.options.jobs = 4;
        assert_eq!(build.ran(&[]).len(), 2);
        remove(&build, &["a.out", "a.meta", "b.out"]);
        let cache = TempCache::new("early-worker-cache");
        build.options.cache = Some(cache.0.clone());
        build.options.workers_only = true;
        let store = TempCache::new("early-worker-store");
        build.options.workers = vec![worker(&store, &["toolchain"])];
        let (report, finished) = build.build(&[]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        assert_eq!(report.remote, 2, "{finished:?}");
        // what needs only the metadata finishes before what wrote it, which is fetched as the worker tells of it
        let order: Vec<&str> = finished.iter().map(|(output, _)| output.as_str()).collect();
        assert_eq!(order, ["b.out", "a.out"]);
        assert_eq!((build.read("b.out"), build.read("a.out")), ("meta\n".into(), "all\n".into()));
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        // with only the targets fetched, the metadata is left there, from which what needs it starts there all the same
        remove(&build, &["a.out", "a.meta", "b.out"]);
        build.options.download = Download::Targets;
        let (cache, store) = (TempCache::new("early-worker-left"), TempCache::new("early-worker-leaves"));
        build.options.cache = Some(cache.0.clone());
        build.options.workers = vec![worker(&store, &["toolchain"])];
        let (report, finished) = build.build(&["b.out"]);
        assert!(report.succeeded(), "{report:?} {finished:?}");
        let order: Vec<&str> = finished.iter().map(|(output, _)| output.as_str()).collect();
        assert_eq!((order, report.remote), (vec!["b.out", "a.out"], 2));
        assert_eq!(build.read("b.out"), "meta\n");
        assert!(!build.dir.join("a.meta").exists() && !build.dir.join("a.out").exists());
        assert_eq!(build.ran(&["b.out"]), Vec::<String>::new());
    }

    #[test]
    fn interrupts_what_runs_on_workers() {
        let mut build = Build::new("interrupts", copied(), &[("a", "0\n"), ("b", "0\n")]);
        let first = TempCache::new("interrupts-first");
        build.options.cache = Some(first.0.clone());
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
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
        let hidden = "mkdir running && cat $in hidden > $out && rmdir running && echo $out: $in > $out.d";
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
        build.quicker_than("slow", &["c.o"]);
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
        let cc = "cat $in extra.h > $out && echo $out: $in extra.h > $out.d && echo compiled $out";
        let hidden = "cat $in hidden > $out && echo $out: $in > $out.d";
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
        // with the one job the build has later, so that how long each edge took is measured without the others
        // starting at once, which on macOS takes the test helper long enough to reorder them
        build.options.jobs = 1;
        build.ran(&[]);
        build.quicker_than("slow", &["a.o", "c.o"]);
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
            let (sub, dir) = (format!("{}/sub", dir.display()), dir.display().to_string());
            let exec = crate::test_helper::exec(vec!["strip".into(), sub, dir, ">".into(), "out".into()]);
            let mut graph = Graph::new();
            let out = graph.file("out");
            let kind = EdgeKind::Command(exec);
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

    /// A graph of one edge, `out`, which writes `RIG_TEST_PASSED` as it sees it, passing on `pass_env`, and cached if
    /// `cached`.
    fn environment(cached: bool, pass_env: &[&str]) -> Graph {
        let exec = crate::test_helper::exec(crate::test_helper::words("getenv RIG_TEST_PASSED > out"));
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(exec);
        let pass_env = pass_env.iter().map(|&name| name.to_string()).collect();
        let cache = cached.then(|| "id".to_string());
        graph.add_edge(crate::Edge { kind, outputs: vec![out], cache, pass_env, ..Default::default() }).unwrap();
        graph
    }

    #[test]
    fn runs_cached_edges_in_a_hermetic_environment() {
        let names_in = |printed: &str| {
            let mut names: Vec<String> =
                printed.lines().filter_map(|line| Some(line.split_once('=')?.0.to_string())).collect();
            names.sort();
            names
        };
        let mut helper = Command::new(crate::test_helper::program());
        let printed = helper.args([crate::test_helper::FLAG, "env"]).env_clear().output().unwrap().stdout;
        let system = names_in(&String::from_utf8_lossy(&printed));
        // the names of the variables the helper's `env` is run with, as it prints them, passing on `pass_env`, and
        // cached if `cached`
        let names = |name: &str, cached: bool, pass_env: &[&str], locating: &[&str]| {
            let exec = crate::test_helper::exec(vec!["env".into()]);
            let mut graph = Graph::new();
            let out = graph.file("out");
            let kind = EdgeKind::Command(exec);
            let pass_env = pass_env.iter().map(|&name| name.to_string()).collect();
            let cache = cached.then(|| "id".to_string());
            graph.add_edge(crate::Edge { kind, outputs: vec![out], cache, pass_env, ..Default::default() }).unwrap();
            let mut build = Build::new(name, graph, &[]);
            build.options.locating = locating.iter().map(|&name| name.to_string()).collect();
            build.options.jobserver = false;
            build.options.env.push(("RIG_TEST_GIVEN".into(), "given".into()));
            let (_, finished) = build.build(&[]);
            let mut names = names_in(&finished[0].1);
            // but for those the system gives every process, as Wine does
            names.retain(|name| !system.contains(name));
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
    #[cfg(unix)]
    fn caches_programs_as_executable() {
        use std::os::unix::fs::PermissionsExt;
        let cache = TempCache::new("executable");
        let exec = crate::test_helper::exec(crate::test_helper::words("printf '#!/bin/sh\\n' > out && chmod-x out"));
        let mut graph = Graph::new();
        let out = graph.file("out");
        let kind = EdgeKind::Command(exec);
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
                ("command", "restat $out $in"),
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
                ("command", "restat $out $in"),
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
            .rule("first", &[("command", "first $in > $out")])
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
        let mut console = Console::new(Vec::new(), false, false, true);
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        let out = String::from_utf8(console.out).unwrap();
        let bad = build.graph.producer(build.graph.node("bad").unwrap()).unwrap();
        let EdgeKind::Command(failed) = build.graph.edge(bad).kind() else {
            unreachable!()
        };
        let line = failed.command_line();
        assert_eq!(out, format!("[1/2] cat a b > ab\n[2/2] FAIL bad\nFAILED: bad\n{line}\nwhy\n"));
    }

    #[test]
    #[cfg(unix)]
    fn cancelling_stops_the_commands() {
        // a command whose process starts another, which is in its process group
        let graph = graph().rule("wait", &[("command", "spawn 30 $out.pid && touch $out")])
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
    #[cfg(unix)]
    fn commands_share_the_jobs() {
        // the only edge running takes the tokens of the other jobs, as a compiler's threads would, and gives them back
        let graph = graph()
            .rule("hold", &[("command", "tokens > $out")])
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
            .edge_with("sleep", &["b"], &[], &[("time", "1.2")])
            .edge_with("sleep", &["c"], &[], &[("time", "0.6")]);
        let mut build = Build::new("implicit-job", graph.finish(), &[]);
        build.options.jobs = 2;
        // so `c` finishes before `b`, about 0.65 s after the build starts, as `b` does at 1.2 s, rather than after it, at
        // 1.8 s, as it would waiting for `b`'s token, whatever starting the test helper takes, as it is slow where it
        // runs alongside the other tests on Windows on Arm, which a time can't allow for
        assert_eq!(build.ran(&[]), ["a", "c", "b"]);
    }

    #[test]
    fn runs_without_a_jobserver() {
        let graph = graph().rule("check", &[("command", "getenv MAKEFLAGS > $out")]).edge("check", &["a"], &[]);
        let mut build = Build::new("no-jobserver", graph.finish(), &[]);
        build.options.jobserver = false;
        // a jobserver which rig is run by is in the environment either way
        if std::env::var_os("MAKEFLAGS").is_none() {
            build.ran(&[]);
            assert_eq!(build.read("a"), "unset\n");
        }
    }
}
