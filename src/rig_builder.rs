//! Runs rig on the graph [`crate::rig_graph`] writes, `graph` in the build directory. The state of the build is kept
//! in `.rig` beside it.
//!
//! `jig build` passes on its options: `-j <jobs>`, `-k <failures>`, `-n` for a dry run, which shows why each edge
//! would run, `-v` to show commands rather than descriptions, and `-q` or `--quiet`, of which the commands which
//! build first take `-j`, `-k` and `-q`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{Error, rig_graph};

/// Where the output of a build is written.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Stdout,
    /// Stderr, so that stdout is left for what follows, such as a program which is run, or JSON diagnostics.
    Stderr,
}

/// What is built, and how.
#[derive(Default)]
pub struct Request<'a> {
    /// The targets to build, or the defaults if there are none.
    pub targets: &'a [&'a str],
    /// How to build, as the options of the command give it.
    pub build: Options,
    /// Show only what the commands print, rather than the progress of the build.
    pub quiet: bool,
    /// Build what can be built after a command fails, rather than stopping.
    pub keep_going: bool,
    pub output: Output,
    /// Edges whose outcomes are collected rather than shown, as `jig test` shows its tests' itself.
    pub tests: Option<&'a std::cell::RefCell<Tests>>,
}

/// The edges which run tests in a build, by their first outputs, with what each is given beside its action, and what
/// each printed and how it finished, which are collected rather than shown.
#[derive(Default)]
pub struct Tests {
    pub given: HashMap<String, rig::exec::Given>,
    /// What each test which ran printed, how it finished, and how long it ran.
    pub finished: HashMap<String, (rig::exec::Outcome, Vec<u8>, std::time::Duration)>,
    /// How many of the other edges failed, as what the tests need.
    pub others_failed: usize,
}

/// An observer which collects what the edges of tests print and how they finish, rather than passing them on, and
/// passes everything else on to the observer which shows the build. The console still shows a test running and
/// finishing, as an edge which succeeded and printed nothing.
struct Collecting<'a> {
    inner: &'a mut dyn rig::exec::Observer,
    tests: &'a std::cell::RefCell<Tests>,
    started: HashMap<rig::EdgeId, std::time::Instant>,
}
impl Collecting<'_> {
    fn is_test(&self, graph: &rig::Graph, edge: rig::EdgeId) -> bool {
        let output = graph.edge(edge).outputs().first().map(|&node| graph.path(node));
        output.is_some_and(|output| self.tests.borrow().given.contains_key(output))
    }
}
impl rig::exec::Observer for Collecting<'_> {
    fn started(&mut self, graph: &rig::Graph, edge: rig::EdgeId, reason: &rig::plan::Reason, progress: rig::exec::Progress) {
        self.started.insert(edge, std::time::Instant::now());
        self.inner.started(graph, edge, reason, progress);
    }
    fn finished(
        &mut self,
        graph: &rig::Graph,
        edge: rig::EdgeId,
        outcome: &rig::exec::Outcome,
        output: &[u8],
        progress: rig::exec::Progress,
    ) {
        let took = self.started.remove(&edge).map(|started| started.elapsed()).unwrap_or_default();
        if !self.is_test(graph, edge) {
            if !outcome.succeeded() && !matches!(outcome, rig::exec::Outcome::Interrupted) {
                self.tests.borrow_mut().others_failed += 1;
            }
            return self.inner.finished(graph, edge, outcome, output, progress);
        }
        let first = graph.path(graph.edge(edge).outputs()[0]).to_string();
        self.tests.borrow_mut().finished.insert(first, (outcome.clone(), output.to_vec(), took));
        self.inner.finished(graph, edge, &rig::exec::Outcome::Succeeded, b"", progress);
    }
    fn unstarted(&mut self, graph: &rig::Graph, edge: rig::EdgeId) {
        self.started.remove(&edge);
        self.inner.unstarted(graph, edge);
    }
    fn tick(&mut self) {
        self.inner.tick();
    }
    fn done(&mut self) {
        self.inner.done();
    }
}

/// The directory of the state of the build, in the build directory.
pub const STATE: &str = ".rig";

/// rig, which builds in the build directory, with the toolchain the project is configured with.
pub struct Rig<'a> {
    pub build_dir: &'a Path,
    pub toolchain: Option<&'a str>,
    /// How rig caches, as the project was configured.
    pub cache: &'a crate::cache::Settings,
}

/// How a command asks for a build, from its options, which [`crate::Args::build`] reads.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// `-j`: the most commands which run at once.
    pub jobs: Option<usize>,
    /// `-k`: the number of failures the build stops after, `0` for none.
    pub keep_going: Option<usize>,
    /// `-n`: show what would run, and why, rather than building.
    pub dry_run: bool,
    /// `-v`: show the commands rather than their descriptions.
    pub verbose: bool,
    /// `-q`: show only what the commands print.
    pub quiet: bool,
}

/// The graph and state loaded while bringing the graph up to date, kept for the build which follows in the same
/// process when that left the graph unchanged. Loading a large graph takes most of the time of a build with nothing
/// to do, so it is not loaded twice.
static LOADED: std::sync::Mutex<Option<Loaded>> = std::sync::Mutex::new(None);

struct Loaded {
    /// The graph's file, and what it was when it was loaded, from which it is known not to have changed since.
    path: PathBuf,
    file: Option<FileId>,
    graph: rig::Graph,
    state: rig::state::State,
}

/// The identity of a file's contents: its inode, size and modification time, which change when the file is
/// replaced or written.
type FileId = (u64, u64, std::time::SystemTime);

fn file_id(path: &Path) -> Option<FileId> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((crate::platform::inode(&metadata), metadata.len(), metadata.modified().ok()?))
}

/// Close the state which was kept loaded, if any. That unlocks it, which matters before running a program, as the
/// program may start a build of its own.
pub fn release() {
    if let Ok(mut loaded) = LOADED.lock()
        && let Some(loaded) = loaded.take()
    {
        drop(loaded.state);
        std::mem::forget(loaded.graph);
    }
}

impl Rig<'_> {
    fn graph(&self) -> Result<rig::Graph, Error> {
        rig::Graph::load(&self.build_dir.join(rig_graph::FILE)).map_err(Error::Rig)
    }
    /// Returns the graph and state, with what the graph's file was when it was loaded. Those which were kept loaded
    /// are reused if the graph's file has not changed since, and otherwise they are loaded.
    fn loaded(&self) -> Result<(rig::Graph, rig::state::State, Option<FileId>), Error> {
        let path = self.build_dir.join(rig_graph::FILE);
        let left = LOADED.lock().ok().and_then(|mut loaded| loaded.take());
        if let Some(loaded) = left
            && loaded.path == path
            && loaded.file.is_some()
            && loaded.file == file_id(&path)
        {
            return Ok((loaded.graph, loaded.state, loaded.file));
        }
        let file = file_id(&path);
        Ok((self.graph()?, self.state()?, file))
    }
    /// The variables a build gives what it runs, beside those inherited: the toolchain the project is configured
    /// with. The variables an edge read are stamped with these applied.
    fn env(&self) -> Vec<(String, String)> {
        self.toolchain.map(|toolchain| ("RUSTUP_TOOLCHAIN".into(), toolchain.into())).into_iter().collect()
    }
    fn state(&self) -> Result<rig::state::State, Error> {
        rig::state::State::open(&self.build_dir.join(STATE)).map_err(Error::Rig)
    }
    /// The options of a build, with the cache the project was configured with, unless it is off.
    fn options(&self) -> Result<rig::exec::Options, Error> {
        let mut options = rig::exec::Options::new(self.build_dir);
        options.env.extend(self.env());
        // Cached edges inherit these as well as the default locating variables. rustup's proxies, such as `rustc`, need
        // these to find a toolchain, and `RUSTUP_TOOLCHAIN` chooses it. The toolchain's identity is its contents as
        // they were when the project was configured, and the project is configured again when the variable changes.
        // A cached command therefore always runs the toolchain it is identified by, not rustup's default.
        options.locating.extend(["RUSTUP_HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN"].map(String::from));
        options.cache = self.cache.open(true)?;
        // workers run cached edges, whose outputs are fetched into the cache
        if options.cache.is_some() {
            options.workers = self.cache.workers()?;
            options.workers_only = self.cache.workers_only;
            if self.cache.download_targets {
                options.download = rig::exec::Download::Targets;
            }
            options.releases.extend(self.cache.release.clone());
        }
        Ok(options)
    }
    /// Build targets, writing what is shown to `output`, returning whether they were built.
    ///
    /// An error which stops the build before it runs anything, such as an unknown target, is shown on stderr, and the
    /// build has failed.
    fn run(
        &self,
        targets: &[&str],
        options: &rig::exec::Options,
        console: Console,
        output: Output,
        tests: Option<&std::cell::RefCell<Tests>>,
    ) -> Result<bool, Error> {
        let (graph, mut state, file) = self.loaded()?;
        // rustc and clippy always print in colour; the colour is removed if the output does not take it
        let (mut out, colour, fd): (Box<dyn std::io::Write>, bool, i32) = match output {
            Output::Stdout => (Box::new(std::io::stdout()), crate::colour(&std::io::stdout()), 1),
            Output::Stderr => (Box::new(std::io::stderr()), crate::colour(&std::io::stderr()), 2),
        };
        let (quiet, verbose) = (console.quiet, console.verbose);
        let mut console = rig::exec::Console::new(&mut out, quiet, verbose, colour);
        // on a terminal which takes escape sequences, what is running is shown in place
        if colour && let Some(width) = rig::exec::terminal_width(fd) {
            console = console.in_place(width);
        }
        let mut collecting;
        let shown: &mut dyn rig::exec::Observer = match tests {
            Some(tests) => {
                collecting = Collecting { inner: &mut console, tests, started: HashMap::new() };
                &mut collecting
            },
            None => &mut console,
        };
        let built = if crate::timings::enabled() {
            let mut observer = Timed { inner: shown, started: HashMap::new() };
            rig::exec::build(&graph, &mut state, options, targets, &mut observer)
        } else {
            rig::exec::build(&graph, &mut state, options, targets, shown)
        };
        // say how many edges ran on workers, which nothing else shows
        if let Ok(report) = &built
            && report.remote > 0
            && !quiet
        {
            let plural = if report.ran == 1 { "" } else { "s" };
            let _ = writeln!(out, "{} of {} command{plural} ran on workers", report.remote, report.ran);
        }
        // once a day, the cache is collected after a build
        if let Some(cache) = &options.cache {
            crate::cache::report_remote(cache);
            self.cache.collect_if_due(cache);
        }
        let path = self.build_dir.join(rig_graph::FILE);
        if targets == [rig_graph::FILE] && file.is_some() && file == file_id(&path) {
            // bringing the graph up to date did not change it, so keep it loaded for the build which follows
            if let Ok(mut loaded) = LOADED.lock() {
                *loaded = Some(Loaded { path, file, graph, state });
            }
        } else {
            // Close the state, which unlocks it for another build. The graph, which may be large, is deliberately not
            // freed, as the process ends soon and freeing it takes time.
            drop(state);
            std::mem::forget(graph);
        }
        match built {
            Ok(report) => Ok(report.succeeded()),
            Err(error @ (rig::Error::UnknownTarget(_) | rig::Error::MissingInput { .. } | rig::Error::Cycle(_))) => {
                eprintln!("{error}");
                Ok(false)
            },
            Err(error) => Err(Error::Rig(error)),
        }
    }
    /// Returns whether targets are up to date, by planning, which runs nothing.
    fn planned(&self, targets: &[&str]) -> Result<bool, Error> {
        let (graph, state, file) = match self.loaded() {
            Ok(loaded) => loaded,
            // the state is locked by a running build, so the targets may not be up to date
            Err(Error::Rig(rig::Error::Locked(_))) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut stamps = rig::Stamps::new(&graph, self.build_dir).with_env(&self.env());
        let planned = match rig::plan::plan(&graph, &state, &mut stamps, targets) {
            Ok(plan) => Ok(plan.is_empty()),
            Err(rig::Error::UnknownTarget(_) | rig::Error::MissingInput { .. } | rig::Error::Cycle(_)) => Ok(false),
            Err(error) => Err(Error::Rig(error)),
        };
        // planning changed neither the graph nor the state, so keep them loaded for what follows
        drop(stamps);
        if let Ok(mut loaded) = LOADED.lock() {
            *loaded = Some(Loaded { path: self.build_dir.join(rig_graph::FILE), file, graph, state });
        }
        planned
    }
}

#[derive(Clone, Copy)]
struct Console {
    quiet: bool,
    verbose: bool,
}

/// An observer which times each command of a build, for `--timings`, from when it starts until it finishes, and
/// passes everything on to the observer which shows the build.
struct Timed<'a> {
    inner: &'a mut dyn rig::exec::Observer,
    started: HashMap<rig::EdgeId, std::time::Instant>,
}
impl rig::exec::Observer for Timed<'_> {
    fn started(&mut self, graph: &rig::Graph, edge: rig::EdgeId, reason: &rig::plan::Reason, progress: rig::exec::Progress) {
        self.started.insert(edge, std::time::Instant::now());
        self.inner.started(graph, edge, reason, progress);
    }
    fn finished(
        &mut self,
        graph: &rig::Graph,
        edge: rig::EdgeId,
        outcome: &rig::exec::Outcome,
        output: &[u8],
        progress: rig::exec::Progress,
    ) {
        if let Some(started) = self.started.remove(&edge) {
            let data = graph.edge(edge);
            let description = match (data.description(), data.kind()) {
                ("", rig::EdgeKind::Command(action)) => action.command_line(),
                (description, _) => description.to_string(),
            };
            crate::timings::command(description, started.elapsed());
        }
        self.inner.finished(graph, edge, outcome, output, progress);
    }
    fn unstarted(&mut self, graph: &rig::Graph, edge: rig::EdgeId) {
        self.started.remove(&edge);
        self.inner.unstarted(graph, edge);
    }
    fn tick(&mut self) {
        self.inner.tick();
    }
    fn done(&mut self) {
        self.inner.done();
    }
}

impl Rig<'_> {
    /// Build what is requested, returning whether it was built, or failing if the build could not be run.
    pub fn build(&self, request: &Request) -> Result<bool, Error> {
        crate::timings::step("build", || self.build_untimed(request))
    }
    fn build_untimed(&self, request: &Request) -> Result<bool, Error> {
        let build = request.build;
        let mut options = self.options()?;
        if let Some(jobs) = build.jobs {
            options.jobs = jobs.max(1);
        }
        options.keep_going = build.keep_going.unwrap_or(usize::from(!request.keep_going));
        if build.dry_run {
            return self.dry_run(request.targets, request.output);
        }
        if let Some(tests) = request.tests {
            options.given.clone_from(&tests.borrow().given);
        }
        let console = Console { quiet: request.quiet || build.quiet, verbose: build.verbose };
        self.run(request.targets, &options, console, request.output, request.tests)
    }
    /// Generate the graph again if it is out of date. What that shows is written to `output`, unless `quiet`.
    /// Nothing is shown if the graph was up to date.
    pub fn regenerate(&self, quiet: bool, output: Output) -> Result<(), Error> {
        crate::timings::step("configure", || self.regenerate_untimed(quiet, output))
    }
    fn regenerate_untimed(&self, quiet: bool, output: Output) -> Result<(), Error> {
        if self.regenerate_here(quiet, output)? {
            return Ok(());
        }
        let console = Console { quiet, verbose: false };
        if self.run(&[rig_graph::FILE], &self.options()?, console, output, None)? { Ok(()) } else { Err(Error::BuildFailed) }
    }
    /// Returns whether a target is up to date, without building it or generating the graph again.
    pub fn up_to_date(&self, target: &str) -> Result<bool, Error> {
        self.planned(&[target])
    }
    /// Returns whether the graph is up to date, without generating it again.
    pub fn graph_up_to_date(&self) -> Result<bool, Error> {
        self.planned(&[rig_graph::FILE])
    }
}

impl Rig<'_> {
    /// Bring the graph up to date within this process, without running `jig generate`, and return whether that was
    /// done. It is only done if the edge which generates the graph is the only one out of date, and its configure
    /// tool is the `jig` which is running. Otherwise rig builds the graph: a project which builds the tool that
    /// configures it, for example, must have the tool built first.
    ///
    /// The graph and state loaded to plan this are kept for the build which follows. The edge is recorded as up to
    /// date in the state, as rig would record an edge it ran.
    fn regenerate_here(&self, quiet: bool, output: Output) -> Result<bool, Error> {
        let (graph, mut state, file) = match self.loaded() {
            Ok(loaded) => loaded,
            Err(Error::Rig(rig::Error::Locked(_))) => return Ok(false),
            Err(error) => return Err(error),
        };
        let path = self.build_dir.join(rig_graph::FILE);
        let leave = |graph, state, file| {
            if let Ok(mut loaded) = LOADED.lock() {
                *loaded = Some(Loaded { path: path.clone(), file, graph, state });
            }
        };
        let mut stamps = rig::Stamps::new(&graph, self.build_dir).with_env(&self.env());
        let planned = rig::plan::plan(&graph, &state, &mut stamps, &[rig_graph::FILE]);
        drop(stamps);
        let Ok(plan) = planned else {
            leave(graph, state, file);
            return Ok(false);
        };
        if plan.is_empty() {
            leave(graph, state, file);
            return Ok(true);
        }
        let generator = graph.node(rig_graph::FILE).and_then(|node| graph.producer(node));
        let generated = match plan.edges.as_slice() {
            [(edge, _)] if Some(*edge) == generator => generated_by(&graph, *edge),
            _ => None,
        };
        let Some((dir, input, toolchain)) = generated else {
            leave(graph, state, file);
            return Ok(false);
        };
        if !quiet {
            let line = format!("[1/1] CONFIGURE {}\n", rig_graph::FILE);
            let _ = match output {
                Output::Stdout => std::io::Write::write_all(&mut std::io::stdout(), line.as_bytes()),
                Output::Stderr => std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes()),
            };
        }
        // the old graph is deliberately not freed, as the process ends soon; it is loaded again once it is written
        std::mem::forget(graph);
        // configure from the project root, as the edge would, since the configuration's paths are relative to it
        let cwd = std::env::current_dir().map_err(|error| Error::Read(".".into(), error))?;
        std::env::set_current_dir(&dir).map_err(|error| Error::Read(dir.clone(), error))?;
        let generated = crate::generate(input, toolchain.as_deref());
        let restored = std::env::set_current_dir(&cwd).map_err(|error| Error::Read(cwd, error));
        generated?;
        restored?;
        // record the edge as up to date, unless the build directory changed, so that the graph was written elsewhere
        let written = file_id(&path);
        if written == file {
            drop(state);
            return Ok(true);
        }
        let graph = self.graph()?;
        record_generator_in(&graph, self.build_dir, &mut state, &self.env())?;
        leave(graph, state, written);
        Ok(true)
    }
    /// Show what would run, and why, without running it.
    ///
    /// The graph and state are those kept loaded from bringing the graph up to date, which hold the state's lock, as
    /// a build would. Variables are stamped as the build would give them.
    fn dry_run(&self, targets: &[&str], output: Output) -> Result<bool, Error> {
        use std::io::Write;
        let (graph, state, _) = self.loaded()?;
        let mut stamps = rig::Stamps::new(&graph, self.build_dir).with_env(&self.env());
        let plan = match rig::plan::plan(&graph, &state, &mut stamps, targets) {
            Ok(plan) => plan,
            Err(error) => {
                eprintln!("{error}");
                return Ok(false);
            },
        };
        let mut out: Box<dyn Write> = match output {
            Output::Stdout => Box::new(std::io::stdout()),
            Output::Stderr => Box::new(std::io::stderr()),
        };
        for (edge, reason) in &plan.edges {
            let edge = graph.edge(*edge);
            let _ = writeln!(out, "{}: {reason}", edge.description());
        }
        Ok(true)
    }
}

/// Record the edge which generates the graph as up to date, once `jig generate` has written the graph. Without a
/// record, rig would run the edge on the first build, and generate the graph a second time.
///
/// Nothing is recorded if the state is in use. That is the case when rig itself is running the edge, and rig then
/// records it once it has run.
pub fn record_generator(graph: &rig::Graph, build_dir: &Path) -> Result<(), Error> {
    let mut state = match rig::state::State::open(&build_dir.join(STATE)) {
        Ok(state) => state,
        Err(rig::Error::Locked(_)) => return Ok(()),
        Err(error) => return Err(Error::Rig(error)),
    };
    // stamp the variables as the edge which runs `jig generate` would see them
    record_generator_in(graph, build_dir, &mut state, &[])
}

/// Returns how the edge which generates the graph runs its configure tool, if the tool is the `jig` which is
/// running: the directory it runs in, the root build file, and the toolchain, if one is given.
fn generated_by(graph: &rig::Graph, edge: rig::EdgeId) -> Option<(PathBuf, PathBuf, Option<String>)> {
    let data = graph.edge(edge);
    let rig::EdgeKind::Command(exec) = data.kind() else {
        return None;
    };
    let running = crate::configured::jig().ok()?;
    if !data.generator() || !crate::configured::same_file(Path::new(&exec.program), &running) {
        return None;
    }
    let (toolchain, input) = generate_args(&exec.args)?;
    Some((PathBuf::from(exec.dir.as_ref()?), input.into(), toolchain))
}

/// Parses the arguments of `jig [+<toolchain>] generate <input>` into the toolchain and the root build file.
/// Returns `None` if the arguments are not of that form.
fn generate_args(args: &[String]) -> Option<(Option<String>, &str)> {
    let (toolchain, args) = match args.split_first()? {
        (first, rest) if first.starts_with('+') => (Some(first[1..].to_string()), rest),
        _ => (None, args),
    };
    match args {
        [generate, input] if generate == "generate" => Some((toolchain, input)),
        _ => None,
    }
}

/// Record the edge which generates the graph as up to date in a state which is open.
fn record_generator_in(
    graph: &rig::Graph,
    build_dir: &Path,
    state: &mut rig::state::State,
    env: &[(String, String)],
) -> Result<(), Error> {
    let Some(node) = graph.node(rig_graph::FILE) else {
        return Ok(());
    };
    let Some(edge) = graph.producer(node).filter(|&edge| graph.edge(edge).generator()) else {
        return Ok(());
    };
    let mut stamps = rig::Stamps::new(graph, build_dir).with_env(env);
    let inputs = rig::plan::input_stamps(graph, state, edge, &mut stamps);
    let discovered = match graph.edge(edge).depfile() {
        Some(depfile) => {
            let path: PathBuf = build_dir.join(depfile);
            let text = std::fs::read_to_string(&path).map_err(|error| Error::Read(path.clone(), error))?;
            rig::depfile::parse(&text).map_err(|error| Error::Depfile(path, error.clone()))?
        },
        None => rig::depfile::Discovered::default(),
    };
    let now = std::time::SystemTime::now();
    let record = rig::plan::record(graph, edge, inputs, &discovered, &mut stamps, now, std::time::Duration::ZERO);
    state.record(record).map_err(Error::Rig)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_how_the_graph_is_generated() {
        let args = |args: &[&str]| args.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(generate_args(&args(&["generate", "Jig"])), Some((None, "Jig")));
        let toolchain = args(&["+nightly", "generate", "sub/Jig"]);
        assert_eq!(generate_args(&toolchain), Some((Some("nightly".into()), "sub/Jig")));
        // what is not how jig generates it is left to rig to run
        assert_eq!(generate_args(&args(&["build"])), None);
        assert_eq!(generate_args(&args(&["generate", "Jig", "extra"])), None);
        assert_eq!(generate_args(&args(&["+nightly"])), None);
    }
}
