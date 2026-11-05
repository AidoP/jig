//! rig, which builds the graph from `graph` in the build directory, as [`crate::rig_graph`] writes it, keeping the
//! state of the build in `.rig` beside it.
//!
//! It is used rather than ninja with `JIG_ENGINE=rig`. `jig build` takes a subset of ninja's options for it:
//! `-j <jobs>`, `-k <failures>`, `-n` for a dry run, which shows why each edge would run, `-v` to show commands
//! rather than descriptions, and `--quiet`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::builder::{Builder, Output, Request};
use crate::{Error, ninja, rig_graph};

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
    /// Parse the arguments `jig build` passes on, which are ninja's options that rig takes, and targets.
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

impl Rig<'_> {
    fn graph(&self) -> Result<rig::Graph, Error> {
        rig::Graph::load(&self.build_dir.join(rig_graph::FILE)).map_err(Error::Rig)
    }
    fn state(&self) -> Result<rig::state::State, Error> {
        rig::state::State::open(&self.build_dir.join(STATE)).map_err(Error::Rig)
    }
    /// The options of a build, with the cache, unless it is off, as the project was configured.
    fn options(&self) -> Result<rig::exec::Options, Error> {
        let mut options = rig::exec::Options::new(self.build_dir);
        if let Some(toolchain) = self.toolchain {
            options.env.push(("RUSTUP_TOOLCHAIN".into(), toolchain.into()));
        }
        // rustup's proxies, such as `rustc`, find the toolchain with these, and `RUSTUP_TOOLCHAIN` chooses it, which is
        // identified by its contents instead, as it was when the project was configured, which it is again when the
        // variable changes, so that a cached command runs the toolchain it is identified by, rather than rustup's default
        options.locating.extend(["RUSTUP_HOME", "CARGO_HOME", "RUSTUP_TOOLCHAIN"].map(String::from));
        options.cache = self.cache.open(true)?;
        // workers run cached edges, whose outputs are fetched into the cache
        if options.cache.is_some() {
            options.workers = self.cache.workers()?;
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
        let graph = self.graph()?;
        let mut state = self.state()?;
        // rustc and clippy print in colour, which is left out where the output is not in colour, as ninja does
        let (mut out, colour): (Box<dyn std::io::Write>, bool) = match output {
            Output::Stdout => (Box::new(std::io::stdout()), crate::colour(&std::io::stdout())),
            Output::Stderr => (Box::new(std::io::stderr()), crate::colour(&std::io::stderr())),
        };
        let (quiet, verbose) = (console.quiet, console.verbose);
        let mut observer = rig::exec::Console { out: &mut out, quiet, verbose, colour };
        let built = rig::exec::build(&graph, &mut state, options, targets, &mut observer);
        // once a day, the cache is collected after a build
        if let Some(cache) = &options.cache {
            crate::cache::report_remote(cache);
            self.cache.collect_if_due(cache);
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
        let graph = self.graph()?;
        let state = match self.state() {
            Ok(state) => state,
            // a build is running, so they may not be
            Err(Error::Rig(rig::Error::Locked(_))) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut stamps = rig::Stamps::new(&graph, self.build_dir);
        match rig::plan::plan(&graph, &state, &mut stamps, targets) {
            Ok(plan) => Ok(plan.is_empty()),
            Err(rig::Error::UnknownTarget(_) | rig::Error::MissingInput { .. } | rig::Error::Cycle(_)) => Ok(false),
            Err(error) => Err(Error::Rig(error)),
        }
    }
}

#[derive(Clone, Copy)]
struct Console {
    quiet: bool,
    verbose: bool,
}

impl Builder for Rig<'_> {
    fn build(&self, request: &Request) -> Result<bool, Error> {
        let args = Args::parse(request.args)?;
        let mut options = self.options()?;
        if let Some(jobs) = args.jobs {
            options.jobs = jobs.max(1);
        }
        options.keep_going = args.keep_going.unwrap_or(usize::from(!request.keep_going));
        let mut targets: Vec<&str> = args.targets.iter().map(String::as_str).collect();
        targets.extend_from_slice(request.targets);
        if args.dry_run {
            return self.dry_run(&targets, request.output);
        }
        let console = Console { quiet: request.quiet || args.quiet, verbose: args.verbose };
        self.run(&targets, &options, console, request.output)
    }
    fn regenerate(&self, quiet: bool, output: Output) -> Result<(), Error> {
        let console = Console { quiet, verbose: false };
        if self.run(&[ninja::FILE], &self.options()?, console, output)? { Ok(()) } else { Err(Error::BuildFailed) }
    }
    fn up_to_date(&self, target: &str) -> Result<bool, Error> {
        self.planned(&[target])
    }
    fn graph_up_to_date(&self) -> Result<bool, Error> {
        self.planned(&[ninja::FILE])
    }
}

impl Rig<'_> {
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
    let Some(node) = graph.node(ninja::FILE) else {
        return Ok(());
    };
    let Some(edge) = graph.producer(node).filter(|&edge| graph.edge(edge).generator()) else {
        return Ok(());
    };
    let mut state = match rig::state::State::open(&build_dir.join(STATE)) {
        Ok(state) => state,
        Err(rig::Error::Locked(_)) => return Ok(()),
        Err(error) => return Err(Error::Rig(error)),
    };
    let mut stamps = rig::Stamps::new(graph, build_dir);
    let inputs = rig::plan::input_stamps(graph, &state, edge, &mut stamps);
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
    fn parses_ninja_options() {
        let args = parse(&["-j", "4", "-k0", "-v", "a", "--quiet", "-n", "--", "-b"]).unwrap_or_else(|_| panic!());
        assert_eq!((args.jobs, args.keep_going), (Some(4), Some(0)));
        assert!(args.verbose && args.quiet && args.dry_run);
        assert_eq!(args.targets, ["a", "-b"]);
        assert!(matches!(parse(&["-t", "deps"]), Err(Error::RigArgument(arg)) if arg == "-t"));
        assert!(matches!(parse(&["-j", "x"]), Err(Error::RigArgument(arg)) if arg == "-j x"));
    }
}
