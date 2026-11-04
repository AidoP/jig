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
use std::time::{Instant, SystemTime};

use crate::graph::{Action, EdgeId, EdgeKind, Graph, NodeId, Pool};
use crate::plan::{self, Reason};
use crate::stamp::{Stamp, Stamps};
use crate::state::State;
use crate::{Error, depfile};

/// How a build runs.
#[derive(Clone, Debug)]
pub struct Options {
    /// The directory the paths of the graph are relative to, where the commands run.
    pub dir: PathBuf,
    /// The most edges which run at once.
    pub jobs: usize,
    /// The number of edges which may fail before no more are started, or `0` for no limit, as ninja's `-k`.
    pub keep_going: usize,
}
impl Options {
    /// Options for a build in a directory, with as many jobs as the machine runs at once, which stops at the first
    /// failure.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        let jobs = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self { dir: dir.into(), jobs, keep_going: 1 }
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
}
impl Report {
    /// Whether every edge which had to run succeeded.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.failed == 0 && self.not_run == 0
    }
}

/// What a thread which ran an edge sends back.
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
    let mut stamps = Stamps::new(graph, &options.dir);
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
    };
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
}

impl Run<'_, '_> {
    fn run(&mut self, observer: &mut dyn Observer) -> Result<(), Error> {
        let (sender, receiver) = std::sync::mpsc::channel::<Finished>();
        std::thread::scope(|scope| {
            loop {
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
                    self.start(index, observer);
                    let console = self.graph.edge(self.edges[index]).pool() == Pool::Console;
                    let Action::Shell(command) = self.action(index).clone();
                    let (dir, sender) = (self.options.dir.clone(), sender.clone());
                    scope.spawn(move || {
                        let start = SystemTime::now();
                        let clock = Instant::now();
                        let (outcome, output) = run_command(&command, &dir, console);
                        let finished = Finished { index, outcome, output, start, duration: clock.elapsed() };
                        let _ = sender.send(finished);
                    });
                }
                if self.progress.running == 0 {
                    break;
                }
                let Ok(finished) = receiver.recv() else {
                    break;
                };
                self.finish(finished, observer);
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
        failed || self.error.is_some()
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
    /// Whether an edge which depends on restat edges, none of which changed their outputs, is up to date without them.
    fn restat_clean(&mut self, index: usize) -> bool {
        let deps = &self.deps[index];
        let unchanged = |dep: &usize| self.status[*dep] == Status::Done { changed: false };
        if deps.is_empty() || !deps.iter().all(unchanged) || !matches!(self.reasons[index], Reason::InputRebuilt(_)) {
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
                None => Ok(Vec::new()),
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
        if !outcome.succeeded() {
            self.status[index] = Status::Failed;
            self.report.failed += 1;
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

/// Returns the inputs a depfile lists, or none if it is missing, as a command need not write one.
fn read_depfile(path: &Path) -> Result<Vec<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            depfile::parse(&String::from_utf8_lossy(&bytes)).map_err(|error| format!("depfile {path:?}: {error}"))
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("depfile {path:?}: {error}")),
    }
}

/// Run a command with `sh -c` in a directory, returning how it finished and its output, stdout and stderr together.
///
/// A console command has the terminal, and no output is captured, while any other has no stdin.
fn run_command(command: &str, dir: &Path, console: bool) -> (Outcome, Vec<u8>) {
    let mut sh = Command::new("/bin/sh");
    sh.arg("-c").arg(command).current_dir(dir);
    if console {
        return match sh.status() {
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
    sh.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    let child = sh.spawn();
    // the command holds the pipe's writers, which must be closed for the output to end
    drop(sh);
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return (Outcome::Error(format!("failed to run `sh`: {error}")), Vec::new()),
    };
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
/// is shown with its outputs and command. With `quiet`, only the output of the commands and failures are shown.
pub struct Console<W: std::io::Write> {
    pub out: W,
    pub quiet: bool,
}
impl<W: std::io::Write> Console<W> {
    fn status(&mut self, graph: &Graph, edge: EdgeId, progress: Progress) {
        if self.quiet {
            return;
        }
        let data = graph.edge(edge);
        let description = match (data.description(), data.kind()) {
            ("", EdgeKind::Command(Action::Shell(command))) => command.as_str(),
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
        if !outcome.succeeded() {
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
    use std::time::Duration;

    /// A build in a temporary directory, which runs real commands.
    struct Build {
        dir: PathBuf,
        graph: Graph,
        state: State,
        options: Options,
    }
    impl Build {
        fn new(name: &str, ninja: &str, files: &[(&str, &str)]) -> Self {
            let dir = std::env::temp_dir().join(format!("rig-exec-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (path, contents) in files {
                std::fs::write(dir.join(path), contents).unwrap();
            }
            let state = State::open(&dir.join(".rig")).unwrap();
            let options = Options::new(&dir);
            Self { graph: crate::ninja::read(ninja).unwrap(), state, options, dir }
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

    const CAT: &str = "rule cat\n  command = cat $in > $out\n";

    #[test]
    fn builds_and_records() {
        let ninja = format!("{CAT}build ab: cat a b\nbuild abc: cat ab c\n");
        let mut build = Build::new("records", &ninja, &[("a", "a\n"), ("b", "b\n"), ("c", "c\n")]);
        assert_eq!(build.ran(&[]), ["ab", "abc"]);
        assert_eq!(build.read("abc"), "a\nb\nc\n");
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("c", "changed\n");
        assert_eq!(build.ran(&[]), ["abc"]);
        assert_eq!(build.read("abc"), "a\nb\nchanged\n");
    }

    #[test]
    fn runs_in_parallel() {
        let ninja = "rule sleep\n  command = sleep 0.4 && touch $out\nbuild a: sleep\nbuild b: sleep\nbuild c: sleep\n";
        let mut build = Build::new("parallel", ninja, &[]);
        build.options.jobs = 3;
        let start = Instant::now();
        assert_eq!(build.ran(&[]).len(), 3);
        assert!(start.elapsed() < Duration::from_millis(1150), "{:?}", start.elapsed());
    }

    #[test]
    fn captures_output() {
        let ninja = "rule say\n  command = echo out && echo err >&2 && touch $out\nbuild a: say\n";
        let mut build = Build::new("output", ninja, &[]);
        assert_eq!(build.build(&[]).1, [("a".to_string(), "out\nerr\n".to_string())]);
    }

    #[test]
    fn stops_at_a_failure() {
        let ninja = format!(
            "{CAT}rule fail\n  command = echo failing && exit 3\n\
             build bad: fail\nbuild after: cat bad\nbuild other: cat a\nbuild more: cat a\n"
        );
        let mut build = Build::new("failure", &ninja, &[("a", "a\n")]);
        build.options.jobs = 1;
        let (report, finished) = build.build(&[]);
        assert_eq!(finished, [("bad".to_string(), "failing\n".to_string())]);
        assert_eq!(report, Report { ran: 1, failed: 1, not_run: 3, restat_skipped: 0 });
        // with `-k 0`, what does not depend on the failure is built
        build.options.keep_going = 0;
        let (report, finished) = build.build(&[]);
        assert_eq!(finished.len(), 3);
        assert_eq!(report, Report { ran: 3, failed: 1, not_run: 1, restat_skipped: 0 });
        // the failed edge is not recorded, so it runs again
        assert_eq!(build.build(&[]).0, Report { ran: 1, failed: 1, not_run: 1, restat_skipped: 0 });
    }

    #[test]
    fn records_discovered_inputs() {
        let ninja = "rule cc\n  command = cat $in > $out && echo \"$out: $in extra\\ file.h\" > $out.d\n  \
                     depfile = $out.d\n  deps = gcc\nbuild a.o: cc a.c\n";
        let mut build = Build::new("depfile", ninja, &[("a.c", "a\n"), ("extra file.h", "h\n")]);
        assert_eq!(build.ran(&[]), ["a.o"]);
        assert_eq!(build.ran(&[]), Vec::<String>::new());
        build.write("extra file.h", "changed\n");
        assert_eq!(build.ran(&[]), ["a.o"]);
    }

    #[test]
    fn fails_with_an_invalid_depfile() {
        let ninja = "rule cc\n  command = touch $out && echo nonsense > $out.d\n  depfile = $out.d\nbuild a.o: cc\n";
        let mut build = Build::new("bad-depfile", ninja, &[]);
        assert_eq!(build.build(&[]).0.failed, 1);
    }

    #[test]
    fn restat_skips_what_did_not_change() {
        // the generated file is only written when its contents change, as jig writes generated files
        let ninja = format!(
            "{CAT}rule gen\n  \
             command = cat $in > $out.tmp && (cmp -s $out.tmp $out || mv $out.tmp $out) && rm -f $out.tmp\n  \
             restat = 1\nbuild gen.h: gen gen.in\nbuild a.o: cat gen.h\nbuild app: cat a.o\n"
        );
        let mut build = Build::new("restat", &ninja, &[("gen.in", "same\n")]);
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
        let ninja = "rule alone\n  command = mkdir running && sleep 0.1 && rmdir running && touch $out\n  \
                     pool = console\n\
                     build a: alone\nbuild b: alone\nbuild c: alone\n";
        let mut build = Build::new("console", ninja, &[]);
        build.options.jobs = 3;
        let (report, _) = build.build(&[]);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.ran, 3);
    }

    #[test]
    fn shows_progress_as_ninja_does() {
        let fail = "rule fail\n  command = echo why && false\n  description = FAIL $out\n";
        let ninja = format!("{CAT}{fail}build ab: cat a b\nbuild bad: fail\n");
        let mut build = Build::new("console-output", &ninja, &[("a", "a\n"), ("b", "b\n")]);
        build.options.jobs = 1;
        let mut console = Console { out: Vec::new(), quiet: false };
        super::build(&build.graph, &mut build.state, &build.options, &[], &mut console).unwrap();
        let out = String::from_utf8(console.out).unwrap();
        assert_eq!(out, "[1/2] cat a b > ab\n[2/2] FAIL bad\nFAILED: bad\necho why && false\nwhy\n");
    }
}
