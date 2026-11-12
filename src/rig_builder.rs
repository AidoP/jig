//! rig, which builds the graph from `graph` in the build directory, as [`crate::rig_graph`] writes it, keeping the
//! state of the build in `.rig` beside it.
//!
//! `jig build` takes its options: `-j <jobs>`, `-k <failures>`, `-n` for a dry run, which shows why each edge would
//! run, `-v` to show commands rather than descriptions, and `--quiet`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::builder::{Builder, Output, Request};
use crate::{Error, rig_graph};

/// The directory of the state of the build, in the build directory.
pub const STATE: &str = ".rig";

/// rig, which builds in the build directory, with the toolchain the project is configured with.
pub struct Rig<'a> {
    pub build_dir: &'a Path,
    pub toolchain: Option<&'a str>,
    /// How rig caches, as the project was configured.
    pub cache: &'a crate::cache::Settings,
}

/// How `jig build` asks for a build, from its arguments.
#[derive(Default)]
struct Args {
    jobs: Option<usize>,
    keep_going: Option<usize>,
    dry_run: bool,
    verbose: bool,
    quiet: bool,
    targets: Vec<String>,
}

impl Args {
    /// Parse the arguments `jig build` passes on, which are rig's options, and targets.
    fn parse(args: &[OsString]) -> Result<Self, Error> {
        let mut parsed = Self::default();
        let mut args = args.iter().map(|arg| arg.to_string_lossy().into_owned());
        while let Some(arg) = args.next() {
            let mut number = |option: &str, inline: &str| -> Result<usize, Error> {
                let value = if inline.is_empty() { args.next().unwrap_or_default() } else { inline.to_string() };
                value.parse().map_err(|_| Error::RigArgument(format!("{option} {value}")))
            };
            match arg.as_str() {
                "-n" => parsed.dry_run = true,
                "-v" | "--verbose" => parsed.verbose = true,
                "--quiet" => parsed.quiet = true,
                "--" => parsed.targets.extend(args.by_ref()),
                _ if arg.starts_with("-j") => parsed.jobs = Some(number("-j", &arg[2..])?),
                _ if arg.starts_with("-k") => parsed.keep_going = Some(number("-k", &arg[2..])?),
                _ if arg.starts_with('-') => return Err(Error::RigArgument(arg)),
                _ => parsed.targets.push(arg),
            }
        }
        Ok(parsed)
    }
}

/// The graph and state which bringing the graph up to date loaded, while the graph is as it was, which the build which
/// follows it in the same process takes, rather than loading them again, as loading a large graph takes most of a build
/// which has nothing to do.
static LOADED: std::sync::Mutex<Option<Loaded>> = std::sync::Mutex::new(None);

struct Loaded {
    /// The graph's file, and what it was when it was loaded, by which it is known not to have changed.
    path: PathBuf,
    file: Option<FileId>,
    graph: rig::Graph,
    state: rig::state::State,
}

/// What a file is: its inode, size and modification time, which change when it is replaced or written.
type FileId = (u64, u64, std::time::SystemTime);

fn file_id(path: &Path) -> Option<FileId> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((crate::platform::inode(&metadata), metadata.len(), metadata.modified().ok()?))
}

/// Close the state which was left loaded, if it was, which unlocks it for another build, as a program which runs may
/// start one.
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
    /// Returns the graph and state, as they were left loaded if the graph has not changed since, or loaded, with what
    /// the graph's file was when it was loaded.
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
    /// The variables a build gives what it runs, beside those it inherits, by which it stamps the variables an edge
    /// read: the toolchain the project is configured with.
    fn env(&self) -> Vec<(String, String)> {
        self.toolchain.map(|toolchain| ("RUSTUP_TOOLCHAIN".into(), toolchain.into())).into_iter().collect()
    }
    fn state(&self) -> Result<rig::state::State, Error> {
        rig::state::State::open(&self.build_dir.join(STATE)).map_err(Error::Rig)
    }
    /// The options of a build, with the cache, unless it is off, as the project was configured.
    fn options(&self) -> Result<rig::exec::Options, Error> {
        let mut options = rig::exec::Options::new(self.build_dir);
        options.env.extend(self.env());
        // rustup's proxies, such as `rustc`, find the toolchain with these, and `RUSTUP_TOOLCHAIN` chooses it, which is
        // identified by its contents instead, as it was when the project was configured, which it is again when the
        // variable changes, so that a cached command runs the toolchain it is identified by, rather than rustup's default
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
    ) -> Result<bool, Error> {
        let (graph, mut state, file) = self.loaded()?;
        // rustc and clippy print in colour, which is left out where the output is not in colour
        let (mut out, colour, fd): (Box<dyn std::io::Write>, bool, i32) = match output {
            Output::Stdout => (Box::new(std::io::stdout()), crate::colour(&std::io::stdout()), 1),
            Output::Stderr => (Box::new(std::io::stderr()), crate::colour(&std::io::stderr()), 2),
        };
        let (quiet, verbose) = (console.quiet, console.verbose);
        let mut console = rig::exec::Console::new(&mut out, quiet, verbose, colour);
        // with escape sequences, on a terminal, what is running is listed in place
        if colour && let Some(width) = rig::exec::terminal_width(fd) {
            console = console.in_place(width);
        }
        let built = if crate::timings::enabled() {
            let mut observer = Timed { inner: &mut console, started: HashMap::new() };
            rig::exec::build(&graph, &mut state, options, targets, &mut observer)
        } else {
            rig::exec::build(&graph, &mut state, options, targets, &mut console)
        };
        // what ran elsewhere is told, as it is not otherwise shown
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
            // what brought the graph up to date, without changing it, leaves it for the build which follows
            if let Ok(mut loaded) = LOADED.lock() {
                *loaded = Some(Loaded { path, file, graph, state });
            }
        } else {
            // the state is closed, which unlocks it for another build, while the graph, which may be large, is not
            // freed, as the process ends soon after
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
    /// Returns whether targets are up to date, from a plan, which runs nothing.
    fn planned(&self, targets: &[&str]) -> Result<bool, Error> {
        let (graph, state, file) = match self.loaded() {
            Ok(loaded) => loaded,
            // a build is running, so they may not be
            Err(Error::Rig(rig::Error::Locked(_))) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut stamps = rig::Stamps::new(&graph, self.build_dir).with_env(&self.env());
        let planned = match rig::plan::plan(&graph, &state, &mut stamps, targets) {
            Ok(plan) => Ok(plan.is_empty()),
            Err(rig::Error::UnknownTarget(_) | rig::Error::MissingInput { .. } | rig::Error::Cycle(_)) => Ok(false),
            Err(error) => Err(Error::Rig(error)),
        };
        // what was left loaded is left for what follows, as planning changes neither
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

/// What times each command of a build, for `--timings`, from when it starts until it finishes, before what shows it.
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

impl Builder for Rig<'_> {
    fn build(&self, request: &Request) -> Result<bool, Error> {
        let args = Args::parse(request.args)?;
        let mut options = self.options()?;
        if let Some(jobs) = args.jobs {
            options.jobs = jobs.max(1);
        }
        options.keep_going = args.keep_going.unwrap_or(usize::from(!request.keep_going));
        // as the graph has them, with `/`, which a path on Windows may be written without
        let given = args.targets.iter().map(|target| crate::rig_graph::path_string(Path::new(target)));
        let given: Vec<String> = given.collect();
        let mut targets: Vec<&str> = given.iter().map(String::as_str).collect();
        targets.extend_from_slice(request.targets);
        if args.dry_run {
            return self.dry_run(&targets, request.output);
        }
        let console = Console { quiet: request.quiet || args.quiet, verbose: args.verbose };
        self.run(&targets, &options, console, request.output)
    }
    fn regenerate(&self, quiet: bool, output: Output) -> Result<(), Error> {
        if self.regenerate_here(quiet, output)? {
            return Ok(());
        }
        let console = Console { quiet, verbose: false };
        if self.run(&[rig_graph::FILE], &self.options()?, console, output)? { Ok(()) } else { Err(Error::BuildFailed) }
    }
    fn up_to_date(&self, target: &str) -> Result<bool, Error> {
        self.planned(&[target])
    }
    fn graph_up_to_date(&self) -> Result<bool, Error> {
        self.planned(&[rig_graph::FILE])
    }
}

impl Rig<'_> {
    /// Bring the graph up to date in this process, rather than by running `jig generate`, if it is only the edge which
    /// generates it which is out of date, and its configure tool is the `jig` which is running, returning whether it
    /// did. Otherwise, as when the project builds the tool which configures it, which is built first, rig builds it.
    ///
    /// The graph and state which were loaded to plan it are left for the build which follows, with the edge recorded as
    /// up to date in the state, as rig records an edge it runs.
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
        // the graph is not freed, as the process ends soon after, and is loaded again once it is written
        std::mem::forget(graph);
        // from the project root, as the edge runs it, which the configuration's paths are relative to
        let cwd = std::env::current_dir().map_err(|error| Error::Read(".".into(), error))?;
        std::env::set_current_dir(&dir).map_err(|error| Error::Read(dir.clone(), error))?;
        let generated = crate::generate(input, toolchain.as_deref());
        let restored = std::env::set_current_dir(&cwd).map_err(|error| Error::Read(cwd, error));
        generated?;
        restored?;
        // the edge is recorded as up to date, unless the graph was written elsewhere, as the build directory changed
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
    fn dry_run(&self, targets: &[&str], output: Output) -> Result<bool, Error> {
        use std::io::Write;
        let graph = self.graph()?;
        let state = self.state()?;
        let mut stamps = rig::Stamps::new(&graph, self.build_dir);
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

/// Record the edge which generates the graph as up to date, once `jig generate` has written it, so that the first
/// build does not generate it again, as rig would otherwise, without a record of it.
///
/// Nothing is recorded if the state is in use, as when rig runs the edge itself, and records it once it has run.
pub fn record_generator(graph: &rig::Graph, build_dir: &Path) -> Result<(), Error> {
    let mut state = match rig::state::State::open(&build_dir.join(STATE)) {
        Ok(state) => state,
        Err(rig::Error::Locked(_)) => return Ok(()),
        Err(error) => return Err(Error::Rig(error)),
    };
    // as the edge which ran `jig generate` gives it its variables, which it inherits then
    record_generator_in(graph, build_dir, &mut state, &[])
}

/// Returns how the edge which generates the graph runs the configure tool, if it is the `jig` which is running: the
/// directory it runs in, the root build file, and the toolchain, if it is given one.
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

/// Returns the toolchain and the root build file of the arguments of `jig [+<toolchain>] generate <input>`, or `None`
/// if they are not those.
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
            rig::depfile::parse(&text).map_err(|error| Error::RigArgument(format!("{path:?}: {error}")))?
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

    fn parse(args: &[&str]) -> Result<Args, Error> {
        Args::parse(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_options() {
        let args = parse(&["-j", "4", "-k0", "-v", "a", "--quiet", "-n", "--", "-b"]).unwrap_or_else(|_| panic!());
        assert_eq!((args.jobs, args.keep_going), (Some(4), Some(0)));
        assert!(args.verbose && args.quiet && args.dry_run);
        assert_eq!(args.targets, ["a", "-b"]);
        assert!(matches!(parse(&["-t", "deps"]), Err(Error::RigArgument(arg)) if arg == "-t"));
        assert!(matches!(parse(&["-j", "x"]), Err(Error::RigArgument(arg)) if arg == "-j x"));
    }

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
